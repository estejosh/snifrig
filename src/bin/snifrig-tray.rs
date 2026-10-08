// Snifrig tray icon. Reads status.json written by the monitor and owns no monitoring of its own,
// so its memory never counts against the monitor's budget. Windowless, one thread, a 15 s timer.
#![cfg_attr(windows, windows_subsystem = "windows")]
#[path = "../icon.rs"]
mod icon;
#[path = "../flyout.rs"]
mod flyout;

use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static DIR: OnceLock<PathBuf> = OnceLock::new();
const WM_TRAY: u32 = WM_APP + 1;
static mut ICONS: [HICON; 3] = [null_mut(); 3]; // 0 ok, 1 alert, 2 monitor not running
static mut LAST_ALERT: u64 = 0;
static mut FIRST: bool = true;
static mut TASKBAR_MSG: u32 = 0;
static mut LAST_HEAD: String = String::new();
static mut HEAD_SHOWN: Vec<(String, u64)> = Vec::new(); // headline balloons already shown, with time
static mut LAST_BALLOON: u64 = 0;
static mut SEEN: Vec<String> = Vec::new(); // pending fix ids already announced
static mut MENU_IDS: Vec<String> = Vec::new(); // pending ids behind the current menu's Approve/Dismiss (100+2i, 101+2i)
const MODES: [&str; 4] = ["off", "dry-run", "ask", "auto"]; // menu ids 110..113

struct Pend { id: String, action: String, name: String, pid: u64, why: String }

fn read_pending() -> Vec<Pend> {
    let s = std::fs::read_to_string(dir().join("pending.json")).unwrap_or_default();
    s.split('{').skip(2).filter_map(|c| {
        let id = snifrig::json_str(c, "id")?;
        Some(Pend { id, action: snifrig::json_str(c, "action").unwrap_or_default(), name: snifrig::json_str(c, "name").unwrap_or_default(),
            pid: snifrig::json_num(c, "pid"), why: snifrig::json_str(c, "why").unwrap_or_default() })
    }).collect()
}

fn fixer_exe() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("snifrig-fix.exe"))).filter(|p| p.exists())
}

fn fix_mode() -> String {
    std::fs::read_to_string(dir().join("fix-mode.txt")).map(|s| s.trim().to_string()).unwrap_or_else(|_| "dry-run".into())
}

fn run_fix(args: &[&str]) {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    if let Some(exe) = fixer_exe() {
        let _ = std::process::Command::new(exe).args(args).arg("--dir").arg(dir()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x0800_0000).spawn();
    }
}

fn esc(s: &str) -> String { s.replace('&', "&&") }

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
fn dir() -> &'static PathBuf { DIR.get().unwrap() }
fn put(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

struct Status { level: usize, avail_mb: u64, alerts: u64, last_alert_unix: u64, last_alert: String, severity: u64, headline: String, causes: Vec<String> }

fn read_status() -> Status {
    let s = std::fs::read_to_string(dir().join("status.json")).unwrap_or_default();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let unix = snifrig::json_num(&s, "unix");
    let fresh = unix > 0 && now.saturating_sub(unix) < 180;
    let level = if !fresh { 2 } else if snifrig::json_str(&s, "level").as_deref() == Some("alert") { 1 } else { 0 };
    Status {
        level, avail_mb: snifrig::json_num(&s, "avail_mb"), alerts: snifrig::json_num(&s, "alerts_2h"),
        last_alert_unix: snifrig::json_num(&s, "last_alert_unix"), last_alert: snifrig::json_str(&s, "last_alert").unwrap_or_default(),
        severity: if fresh { snifrig::json_num(&s, "severity") } else { 0 },
        headline: if fresh { snifrig::json_str(&s, "headline").unwrap_or_default() } else { String::new() },
        causes: if fresh { snifrig::verdict::causes_from_status(&s) } else { Vec::new() },
    }
}

/// The hound head on its charcoal tile, with a status dot: green ok, red alert, gray monitor not running.
fn make_icon(level: usize) -> HICON {
    let pal = |c: u8| match c { b'g' => (217u8, 164u8, 65u8), b'c' => (63, 184, 232), _ => (20, 24, 28) };
    let mut px = vec![0u8; 32 * 32 * 4];
    for (i, c) in icon::ICON.bytes().enumerate().take(1024) {
        let (r, g, b) = pal(c);
        px[i * 4] = b; px[i * 4 + 1] = g; px[i * 4 + 2] = r; px[i * 4 + 3] = 255;
    }
    let dot = match level { 0 => (61u8, 220u8, 132u8), 1 => (229, 72, 77), _ => (138, 143, 152) };
    for y in 0..32i32 {
        for x in 0..32i32 {
            let d2 = (x - 25) * (x - 25) + (y - 25) * (y - 25);
            if d2 <= 36 {
                let (r, g, b) = if d2 > 25 { (255u8, 255u8, 255u8) } else { dot };
                let i = ((y * 32 + x) * 4) as usize;
                px[i] = b; px[i + 1] = g; px[i + 2] = r;
            }
        }
    }
    unsafe {
        let color = CreateBitmap(32, 32, 1, 32, px.as_ptr() as *const _);
        let mask = CreateBitmap(32, 32, 1, 1, null());
        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let h = CreateIconIndirect(&ii);
        DeleteObject(color as _);
        DeleteObject(mask as _);
        h
    }
}

unsafe fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = std::mem::zeroed();
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = 1;
    n
}

unsafe fn refresh(hwnd: HWND, add: bool) {
    let st = read_status();
    let mut n = nid(hwnd);
    n.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = ICONS[st.level];
    let tip = match st.level {
        0 => format!("Snifrig: OK, {:.1} GB RAM free", st.avail_mb as f64 / 1024.0),
        1 => format!("Snifrig: {} alert(s). {}", st.alerts, st.last_alert),
        _ => "Snifrig: monitor not running".to_string(),
    };
    let tip = if !st.headline.is_empty() && st.level != 2 { format!("Snifrig: {}", st.headline) } else { tip };
    put(&mut n.szTip, &tip);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let head_ball = {
        let shown = &mut *std::ptr::addr_of_mut!(HEAD_SHOWN);
        shown.retain(|x| now.saturating_sub(x.1) < 7200);
        let last = &mut *std::ptr::addr_of_mut!(LAST_HEAD);
        let changed = *last != st.headline;
        *last = st.headline.clone();
        changed && st.severity >= 1 && !st.headline.is_empty() && now.saturating_sub(LAST_BALLOON) >= 1800 && !shown.iter().any(|x| x.0 == st.headline)
    };
    if head_ball {
        let shown = &mut *std::ptr::addr_of_mut!(HEAD_SHOWN);
        shown.push((st.headline.clone(), now));
        LAST_BALLOON = now;
        n.uFlags |= NIF_INFO;
        put(&mut n.szInfoTitle, "Your PC is slow");
        put(&mut n.szInfo, &match st.causes.first() { Some(c) => format!("{} {}", st.headline, c), None => st.headline.clone() });
        n.dwInfoFlags = NIIF_WARNING;
    }
    if !FIRST && st.level == 1 && st.last_alert_unix > LAST_ALERT && st.headline.is_empty() {
        n.uFlags |= NIF_INFO;
        put(&mut n.szInfoTitle, "Snifrig alert");
        put(&mut n.szInfo, &st.last_alert);
        n.dwInfoFlags = NIIF_WARNING;
    }
    if st.last_alert_unix > LAST_ALERT { LAST_ALERT = st.last_alert_unix; }
    let seen = &mut *std::ptr::addr_of_mut!(SEEN);
    if let Some(p) = read_pending().into_iter().find(|p| !seen.contains(&p.id)) {
        n.uFlags |= NIF_INFO;
        put(&mut n.szInfoTitle, "Snifrig fixer");
        put(&mut n.szInfo, &format!("Snifrig wants to {} {}. Right-click to approve.", p.action, p.name));
        n.dwInfoFlags = NIIF_WARNING;
    }
    for p in read_pending() { if !seen.contains(&p.id) { seen.push(p.id); } }
    FIRST = false;
    Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &n);
}

unsafe fn open(file: &str, params: Option<&str>) {
    let (f, p) = (wide(file), params.map(wide));
    ShellExecuteW(null_mut(), wide("open").as_ptr(), f.as_ptr(), p.as_ref().map_or(null(), |v| v.as_ptr()), null(), SW_SHOWNORMAL);
}

unsafe fn open_report() {
    let exe = dir().join("bin").join("snifrig.exe");
    open("cmd.exe", Some(&format!("/k \"\"{}\" --once --dir \"{}\"\"", exe.display(), dir().display())));
}

static mut RESTARTS: Vec<u64> = Vec::new();

// Restart the hidden monitor if it died: status is stale, its mutex is free, and the user did not stop it. Max 3 per hour.
unsafe fn watchdog() {
    if read_status().level != 2 || dir().join("stop").exists() { return; }
    let h = windows_sys::Win32::System::Threading::OpenMutexW(0x0010_0000, 0, wide("Global\\snifrig-monitor").as_ptr());
    if !h.is_null() { windows_sys::Win32::Foundation::CloseHandle(h); return; }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let r = &mut *std::ptr::addr_of_mut!(RESTARTS);
    r.retain(|&t| now - t < 3600);
    if r.len() >= 3 { return; }
    if let Some(exe) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("snifrigd.exe"))) {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        if std::process::Command::new(exe).arg("--dir").arg(dir()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x0000_0008 | 0x0800_0000).spawn().is_ok() { r.push(now); }
    }
}

unsafe fn menu(hwnd: HWND) {
    let st = read_status();
    let m = CreatePopupMenu();
    let head = if st.headline.is_empty() { format!("Snifrig: {}", ["OK", "ALERT", "monitor not running"][st.level]) } else { esc(&st.headline) };
    AppendMenuW(m, MF_STRING | MF_GRAYED, 0, wide(&head).as_ptr());
    AppendMenuW(m, MF_SEPARATOR, 0, null());
    AppendMenuW(m, MF_STRING, 1, wide("Show report").as_ptr());
    AppendMenuW(m, MF_STRING, 2, wide("Open alerts").as_ptr());
    AppendMenuW(m, MF_STRING, 3, wide("Open data folder").as_ptr());
    AppendMenuW(m, MF_SEPARATOR, 0, null());
    AppendMenuW(m, MF_STRING, 6, wide("Pause fixing 1 hour").as_ptr());
    AppendMenuW(m, MF_STRING, 7, wide("Resume fixing").as_ptr());
    AppendMenuW(m, MF_SEPARATOR, 0, null());
    if fixer_exe().is_none() {
        AppendMenuW(m, MF_STRING | MF_GRAYED, 0, wide("Fixer not installed").as_ptr());
    } else if !dir().join("snifrig-fix.key").exists() {
        AppendMenuW(m, MF_STRING | MF_GRAYED, 0, wide("Fixer: needs a key").as_ptr());
        AppendMenuW(m, MF_STRING, 8, wide("Fixer pricing").as_ptr());
    } else {
        let ids = &mut *std::ptr::addr_of_mut!(MENU_IDS);
        ids.clear();
        for (i, p) in read_pending().into_iter().take(3).enumerate() {
            let sub = CreatePopupMenu();
            let why: String = p.why.chars().take(60).collect();
            if !why.is_empty() { AppendMenuW(sub, MF_STRING | MF_GRAYED, 0, wide(&esc(&why)).as_ptr()); }
            AppendMenuW(sub, MF_STRING, 100 + 2 * i, wide("Approve").as_ptr());
            AppendMenuW(sub, MF_STRING, 101 + 2 * i, wide("Dismiss").as_ptr());
            let label = if p.pid > 0 { format!("Fix: {} {} (pid {})", p.action, p.name, p.pid) } else { format!("Fix: {} {}", p.action, p.name) };
            AppendMenuW(m, MF_STRING | MF_POPUP, sub as usize, wide(&esc(&label)).as_ptr());
            ids.push(p.id);
        }
        let fm = CreatePopupMenu();
        let cur = fix_mode();
        for (i, md) in MODES.iter().enumerate() {
            AppendMenuW(fm, MF_STRING | if *md == cur { MF_CHECKED } else { 0 }, 110 + i, wide(md).as_ptr());
        }
        AppendMenuW(m, MF_STRING | MF_POPUP, fm as usize, wide("Fixer mode").as_ptr());
    }
    AppendMenuW(m, MF_SEPARATOR, 0, null());
    AppendMenuW(m, MF_STRING, 4, wide("Quit tray icon").as_ptr());
    AppendMenuW(m, MF_STRING, 5, wide("Stop monitor and quit").as_ptr());
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    TrackPopupMenu(m, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(m);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let ev = (lp as u32) & 0xFFFF;
            if ev == WM_RBUTTONUP || ev == WM_CONTEXTMENU { menu(hwnd); } else if ev == WM_LBUTTONUP { flyout::show(dir()); }
            0
        }
        WM_TIMER => {
            let flag = dir().join("stop-tray");
            if flag.exists() { let _ = std::fs::remove_file(&flag); DestroyWindow(hwnd); } else { watchdog(); refresh(hwnd, false); }
            0
        }
        WM_COMMAND => {
            match (wp & 0xFFFF) as u32 {
                1 => open_report(),
                2 => {
                    let p = dir().join("alerts.jsonl");
                    let _ = std::fs::OpenOptions::new().create(true).append(true).open(&p);
                    open("notepad.exe", Some(&format!("\"{}\"", p.display())));
                }
                3 => open("explorer.exe", Some(&format!("\"{}\"", dir().display()))),
                6 => { let u = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) + 3600; let _ = std::fs::write(dir().join("mode.json"), format!("{{\"paused_until\":{}}}", u)); }
                7 => { let _ = std::fs::remove_file(dir().join("mode.json")); }
                8 => open("https://github.com/estejosh/snifrig/blob/main/PRICING.md", None), // only on the user's click
                c @ 100..=105 => {
                    let ids = &*std::ptr::addr_of!(MENU_IDS);
                    if let Some(id) = ids.get(((c - 100) / 2) as usize) { run_fix(&[if c % 2 == 0 { "approve" } else { "dismiss" }, id]); }
                }
                c @ 110..=113 => run_fix(&["mode", MODES[(c - 110) as usize]]),
                4 => { DestroyWindow(hwnd); }
                5 => { let _ = std::fs::write(dir().join("stop"), "1"); DestroyWindow(hwnd); }
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            let n = nid(hwnd);
            Shell_NotifyIconW(NIM_DELETE, &n);
            PostQuitMessage(0);
            0
        }
        _ => {
            if TASKBAR_MSG != 0 && msg == TASKBAR_MSG { refresh(hwnd, true); return 0; } // explorer restarted: re-add the icon
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

fn main() {
    unsafe { SetProcessDPIAware(); }
    let a: Vec<String> = std::env::args().collect();
    let d = a.iter().position(|x| x == "--dir").and_then(|i| a.get(i + 1)).map(PathBuf::from).unwrap_or_else(snifrig::default_dir);
    let _ = std::fs::create_dir_all(&d);
    DIR.set(d).ok();
    unsafe {
        // one tray per data dir: the default dir keeps the plain name, a custom --dir (tests) gets its own
        let mn = wide(&if dir().display().to_string().eq_ignore_ascii_case(&snifrig::default_dir().display().to_string()) { "Global\\snifrig-tray".to_string() } else { format!("Global\\snifrig-tray-{}", dir().display().to_string().replace(|c: char| !c.is_ascii_alphanumeric(), "_")) });
        CreateMutexW(null(), 0, mn.as_ptr());
        if GetLastError() == 183 { return; }
        let hi = GetModuleHandleW(null());
        let cls = wide("SnifrigTray");
        let wc = WNDCLASSW {
            style: 0, lpfnWndProc: Some(wndproc), cbClsExtra: 0, cbWndExtra: 0, hInstance: hi, hIcon: null_mut(),
            hCursor: null_mut(), hbrBackground: null_mut(), lpszMenuName: null(), lpszClassName: cls.as_ptr(),
        };
        RegisterClassW(&wc);
        let title = wide("Snifrig");
        let hwnd = CreateWindowExW(0, cls.as_ptr(), title.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hi, null());
        for i in 0..3 { ICONS[i] = make_icon(i); }
        TASKBAR_MSG = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        refresh(hwnd, true);
        SetTimer(hwnd, 1, 15000, None);
        let fly = a.iter().any(|x| x == "--flyout");
        if fly { flyout::show(dir()); }
        else if std::env::var("SNIFRIG_NO_NOTICE_TEST").as_deref() != Ok("1") { snifrig::notice::show(); } // UFL 2D Notice, once per tray run
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

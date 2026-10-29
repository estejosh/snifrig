// Snifrig tray icon. Reads status.json written by the monitor and owns no monitoring of its own,
// so its memory never counts against the monitor's budget. Windowless, one thread, a 15 s timer.
#[path = "icon.rs"]
mod icon;
#[path = "flyout.rs"]
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
static mut TRAY_START: u64 = 0;
static mut LAST_IMPACT_DAY: u64 = 0;
static mut SEEN: Vec<String> = Vec::new(); // pending fix ids already announced
static mut MENU_IDS: Vec<String> = Vec::new(); // pending ids behind the current menu's Approve/Dismiss (100+2i, 101+2i)
static mut GOV_CMDS: Vec<Vec<String>> = Vec::new(); // fixer args behind Governor menu ids 200+i
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

/// The small "needs your OK" card, if the window app is installed.
/// True while you are in a full-screen app (a game, a video, a presentation) or Focus is on:
/// snifrig then shows nothing at all, and catches up when you come back.
fn user_busy() -> bool {
    let mut st = 0i32;
    unsafe { windows_sys::Win32::UI::Shell::SHQueryUserNotificationState(&mut st) == 0 && st != 5 && st != 0 }
}

fn open_ask() -> bool {
    use std::os::windows::process::CommandExt;
    let Some(exe) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("snifrig-app.exe"))).filter(|p| p.exists()) else { return false };
    std::process::Command::new(exe).arg("--ask").arg("--dir").arg(dir()).creation_flags(0x0000_0008).spawn().is_ok()
}

/// The snifrig window, if it was installed next to the tray.
fn open_app() -> bool {
    use std::os::windows::process::CommandExt;
    let Some(exe) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("snifrig-app.exe"))).filter(|p| p.exists()) else { return false };
    std::process::Command::new(exe).arg("--dir").arg(dir()).creation_flags(0x0000_0008).spawn().is_ok()
}

/// Windows 11 hides new tray icons in the overflow. Show ours on the taskbar once, a few
/// seconds after it appears; if the user hid it again (IsPromoted = 0), leave it hidden.
fn promote_icon() {
    use std::os::windows::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.display().to_string().replace('\'', "''");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let ps = format!("Get-ChildItem 'HKCU:\\Control Panel\\NotifyIconSettings' | Where-Object {{ $p = Get-ItemProperty $_.PSPath; $p.ExecutablePath -eq '{}' -and $null -eq $p.IsPromoted }} | ForEach-Object {{ Set-ItemProperty $_.PSPath -Name IsPromoted -Value 1 -Type DWord }}", exe);
        let _ = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &ps]).creation_flags(0x0800_0000).status();
    });
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

/// The real snifrig icon (embedded resource 1) at the tray's size, as BGRA pixels, top row first.
unsafe fn resource_pixels(size: i32) -> Option<Vec<u8>> {
    use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
    let h = LoadImageW(GetModuleHandleW(null()), 1 as _, IMAGE_ICON, size, size, LR_DEFAULTCOLOR);
    if h.is_null() { return None; }
    let mut ii: ICONINFO = std::mem::zeroed();
    let ok = GetIconInfo(h as HICON, &mut ii) != 0;
    DestroyIcon(h as HICON);
    if !ok { return None; }
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader = BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: size, biHeight: -size, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB as u32, ..std::mem::zeroed() };
    let mut px = vec![0u8; (size * size * 4) as usize];
    let dc = GetDC(null_mut());
    let lines = GetDIBits(dc, ii.hbmColor, 0, size as u32, px.as_mut_ptr() as *mut _, &mut bmi, DIB_RGB_COLORS);
    ReleaseDC(null_mut(), dc);
    DeleteObject(ii.hbmColor as _);
    DeleteObject(ii.hbmMask as _);
    if lines == 0 || px.chunks(4).all(|p| p[3] == 0) { return None; }
    Some(px)
}

/// The hound with a status dot in the corner: green ok, red alert, gray monitor not running.
fn make_icon(level: usize) -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64);
    let px = unsafe { resource_pixels(size) };
    let (size, mut px) = match px {
        Some(p) => (size, p),
        None => {
            // Fallback: the old drawn hound at 32 px.
            let pal = |c: u8| match c { b'g' => (217u8, 164u8, 65u8), b'c' => (63, 184, 232), _ => (20, 24, 28) };
            let mut p = vec![0u8; 32 * 32 * 4];
            for (i, c) in icon::ICON.bytes().enumerate().take(1024) {
                let (r, g, b) = pal(c);
                p[i * 4] = b; p[i * 4 + 1] = g; p[i * 4 + 2] = r; p[i * 4 + 3] = 255;
            }
            (32, p)
        }
    };
    let dot = match level { 0 => (61u8, 220u8, 132u8), 1 => (229, 72, 77), _ => (138, 143, 152) };
    let s = size as f32;
    let (cx, cy, r) = (s * 0.77, s * 0.77, s * 0.2);
    for y in 0..size {
        for x in 0..size {
            let d = (((x as f32 + 0.5) - cx).powi(2) + ((y as f32 + 0.5) - cy).powi(2)).sqrt();
            if d <= r + 1.0 {
                let (rr, gg, bb) = if d > r - s / 16.0 { (255u8, 255u8, 255u8) } else { dot };
                let i = ((y * size + x) * 4) as usize;
                px[i] = bb; px[i + 1] = gg; px[i + 2] = rr; px[i + 3] = 255;
            }
        }
    }
    unsafe {
        let color = CreateBitmap(size, size, 1, 32, px.as_ptr() as *const _);
        let mask = CreateBitmap(size, size, 1, 1, null());
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
    let g = if fixer_exe().is_some() && dir().join("snifrig-fix.key").exists() { flyout::gov_read(dir()) } else { None };
    let gn = g.as_ref().map_or(0, |g| g.held.len());
    let game = g.and_then(|g| g.game);
    let tip = if gn > 0 || game.is_some() {
        let suf = match &game { Some(n) => format!(" \u{b7} performance mode: {} \u{b7} holding {}", n, gn), None => format!(" \u{b7} governor holding {}", gn) };
        let base: String = tip.chars().take(127usize.saturating_sub(suf.encode_utf16().count())).collect();
        format!("{}{}", base, suf)
    } else { tip };
    put(&mut n.szTip, &tip);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let head_ball = {
        let shown = &mut *std::ptr::addr_of_mut!(HEAD_SHOWN);
        shown.retain(|x| now.saturating_sub(x.1) < 7200);
        let last = &mut *std::ptr::addr_of_mut!(LAST_HEAD);
        let changed = *last != st.headline;
        *last = st.headline.clone();
        changed && !user_busy() && st.severity >= 1 && !st.headline.is_empty() && now.saturating_sub(LAST_BALLOON) >= 1800 && !shown.iter().any(|x| x.0 == st.headline)
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
    // Once a day, when you are not busy: what snifrig measured it does for this PC (only a real,
    // counted result; nothing while it is still measuring or found no clear difference).
    if TRAY_START == 0 { TRAY_START = now; }
    if !FIRST && !head_ball && !user_busy() && now.saturating_sub(LAST_BALLOON) >= 1800 && now.saturating_sub(TRAY_START) >= 600 && LAST_IMPACT_DAY != now / 86400 {
        LAST_IMPACT_DAY = now / 86400;
        let s = std::fs::read_to_string(dir().join("impact-summary.json")).unwrap_or_default();
        let num = |k: &str| s.split(&format!("\"{}\":", k)).nth(1).and_then(|v| v.split(|c| c == ',' || c == '}').next()).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
        if s.contains("\"ready\":true") && num("faster_pct") >= 2.0 {
            LAST_BALLOON = now;
            n.uFlags |= NIF_INFO;
            put(&mut n.szInfoTitle, "What snifrig did for you");
            put(&mut n.szInfo, &format!("Over the last 7 days your PC responded {:.0}% faster with snifrig, measured over {:.0} hours of your use. The numbers are in the app.", num("faster_pct"), num("hours")));
            n.dwInfoFlags = 1;
        }
    }
    if !FIRST && !user_busy() && st.level == 1 && st.last_alert_unix > LAST_ALERT && st.headline.is_empty() {
        n.uFlags |= NIF_INFO;
        put(&mut n.szInfoTitle, "Snifrig alert");
        put(&mut n.szInfo, &st.last_alert);
        n.dwInfoFlags = NIIF_WARNING;
    }
    if st.last_alert_unix > LAST_ALERT { LAST_ALERT = st.last_alert_unix; }
    // Something new needs the user's OK (a fix, or a suggestion from their AI assistant):
    // show the snifrig card with real buttons; the balloon is only a fallback.
    let seen = &mut *std::ptr::addr_of_mut!(SEEN);
    let mut ids: Vec<(String, String)> = read_pending().into_iter().map(|p| (p.id, format!("Snifrig wants to {} {}.", p.action, p.name))).collect();
    let sug = std::fs::read_to_string(dir().join("ai-suggestions.json")).unwrap_or_default();
    for part in sug.split("\"id\":\"").skip(1) { if let Some(id) = part.split('"').next() { ids.push((id.to_string(), if part.contains("\"source\":\"snifrig\"") { "snifrig has a question for you.".into() } else { "Your AI assistant has a suggestion for snifrig.".into() })); } }
    // In a game or full screen: hold every question (not marked seen) until you are back.
    let busy = user_busy();
    if busy { ids.clear(); }
    if let Some((_, text)) = ids.iter().find(|(id, _)| !seen.contains(id)) {
        if FIRST || !open_ask() {
            n.uFlags |= NIF_INFO;
            put(&mut n.szInfoTitle, "Snifrig needs your OK");
            put(&mut n.szInfo, &format!("{} Double-click the snifrig icon to decide.", text));
            n.dwInfoFlags = NIIF_WARNING;
        }
    }
    for (id, _) in ids { if !seen.contains(&id) { seen.push(id); } }
    FIRST = false;
    Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &n);
    if add { promote_icon(); }
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
/// Bring the fixer back if it is installed, licensed, not asked to stop, and not running.
unsafe fn fixer_watchdog() {
    let d = dir();
    if d.join("stop-fix").exists() || !d.join("snifrig-fix.key").exists() { return; }
    // Stopped on purpose a moment ago (an update): give it two minutes before bringing it back.
    if std::fs::metadata(d.join("fix-stopped")).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map_or(false, |e| e.as_secs() < 120) { return; }
    let h = windows_sys::Win32::System::Threading::OpenMutexW(0x0010_0000, 0, wide("Global\\snifrig-fix").as_ptr());
    if !h.is_null() { windows_sys::Win32::Foundation::CloseHandle(h); return; }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let r = &mut *std::ptr::addr_of_mut!(FIX_RESTARTS);
    r.retain(|&t| now - t < 3600);
    if r.len() >= 3 {
        // The fixer keeps dying: hand your jobs back to Windows so they keep running (once;
        // the fixer takes them back when it is up again).
        if !d.join("jobs-in-windows").exists() && d.join("jobs.txt").exists() {
            if let Some(exe) = fixer_exe() {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new(exe).arg("--dir").arg(&d).args(["jobs", "handback"]).creation_flags(0x0800_0000).status();
            }
        }
        return;
    }
    if let Some(exe) = fixer_exe() {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        if std::process::Command::new(exe).arg("--dir").arg(d).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x0000_0008 | 0x0800_0000).spawn().is_ok() { r.push(now); }
    }
}
static mut FIX_RESTARTS: Vec<u64> = Vec::new();

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
    AppendMenuW(m, MF_STRING, 9, wide("Open snifrig").as_ptr());
    SetMenuDefaultItem(m, 9, 0);
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
        let cmds = &mut *std::ptr::addr_of_mut!(GOV_CMDS);
        cmds.clear();
        // Teach it: your word beats the meter, and every answer is kept for learning.
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        AppendMenuW(m, MF_STRING, 200 + cmds.len(), wide("Quick fix now\tCtrl+Alt+Shift+S").as_ptr());
        cmds.push(vec!["quick".into()]);
        // "This app feels slow": the apps showing on a screen, from the fixer's latest round.
        let g = std::fs::read_to_string(dir().join("governor.json")).unwrap_or_default();
        let vis: Vec<String> = g.split("\"visible\":[").nth(1).and_then(|s| s.split(']').next()).map(|s| s.split('"').skip(1).step_by(2).map(String::from).collect()).unwrap_or_default();
        if !vis.is_empty() {
            let am = CreatePopupMenu();
            for n in vis.iter().filter(|n| !n.to_lowercase().starts_with("snifrig")).take(15) {
                AppendMenuW(am, MF_STRING, 200 + cmds.len(), wide(&esc(n.trim_end_matches(".exe"))).as_ptr());
                cmds.push(vec!["slow".into(), n.clone()]);
            }
            AppendMenuW(m, MF_STRING | MF_POPUP, am as usize, wide("An app feels slow").as_ptr());
        }
        AppendMenuW(m, MF_STRING, 200 + cmds.len(), wide("PC feels slow right now").as_ptr());
        cmds.push(vec!["feel".into(), "slow".into()]);
        AppendMenuW(m, MF_STRING, 200 + cmds.len(), wide("PC feels fine").as_ptr());
        cmds.push(vec!["feel".into(), "fine".into()]);
        let lm = CreatePopupMenu();
        let learned: Vec<String> = std::fs::read_to_string(dir().join("learned.json")).unwrap_or_default().lines().filter(|l| l.contains("\"text\"")).map(String::from).collect();
        if learned.is_empty() { AppendMenuW(lm, MF_STRING | MF_GRAYED, 0, wide("Nothing yet: answer its questions and it learns").as_ptr()); }
        for l in learned.iter().take(12) {
            let (text, why) = (snifrig::json_str(l, "text").unwrap_or_default(), snifrig::json_str(l, "why").unwrap_or_default());
            let (name, action) = (snifrig::json_str(l, "name").unwrap_or_default(), snifrig::json_str(l, "action").unwrap_or_default());
            if action.is_empty() { AppendMenuW(lm, MF_STRING | MF_GRAYED, 0, wide(&esc(&format!("{} ({})", text, why))).as_ptr()); continue; }
            let sub = CreatePopupMenu();
            AppendMenuW(sub, MF_STRING | MF_GRAYED, 0, wide(&esc(&why)).as_ptr());
            AppendMenuW(sub, MF_STRING, 200 + cmds.len(), wide("Stop doing this, ask me again").as_ptr());
            cmds.push(vec!["unlearn".into(), name, action]);
            AppendMenuW(lm, MF_STRING | MF_POPUP, sub as usize, wide(&esc(&text)).as_ptr());
        }
        AppendMenuW(m, MF_STRING | MF_POPUP, lm as usize, wide("What snifrig learned").as_ptr());
        if let Some(g) = flyout::gov_read(dir()) {
            let gm = CreatePopupMenu();
            let hd = if g.held.is_empty() { "Nothing held back".to_string() } else { format!("Holding back {} app{}", g.held.len(), if g.held.len() == 1 { "" } else { "s" }) };
            AppendMenuW(gm, MF_STRING | MF_GRAYED, 0, wide(&hd).as_ptr());
            if let Some(n) = &g.game { AppendMenuW(gm, MF_STRING | MF_GRAYED, 0, wide(&format!("Performance mode on for {}", n)).as_ptr()); }
            for h in g.held.iter().take(5) {
                let sub = CreatePopupMenu();
                let lv = match h.level { 1 => "efficiency mode", 2 => "below normal", _ => "idle" };
                let why: String = h.reason.chars().take(50).collect();
                let label = if why.is_empty() { format!("{}: {}", h.name, lv) } else { format!("{}: {} ({})", h.name, lv, why) };
                AppendMenuW(sub, MF_STRING, 200 + cmds.len(), wide("Give it full speed now").as_ptr());
                cmds.push(vec!["governor".into(), "undo".into(), h.pid.to_string()]);
                AppendMenuW(sub, MF_STRING, 200 + cmds.len(), wide("Always run in background").as_ptr());
                cmds.push(vec!["rule".into(), "add".into(), format!("{} background=yes", h.name)]);
                AppendMenuW(gm, MF_STRING | MF_POPUP, sub as usize, wide(&esc(&label)).as_ptr());
            }
            for (name, count) in g.sugg.iter().take(5) {
                let label = format!("Make {} always background (held back {} time{} this week)", name, count, if *count == 1 { "" } else { "s" });
                AppendMenuW(gm, MF_STRING, 200 + cmds.len(), wide(&esc(&label)).as_ptr());
                cmds.push(vec!["rule".into(), "add".into(), format!("{} background=yes", name)]);
            }
            AppendMenuW(m, MF_STRING | MF_POPUP, gm as usize, wide("Governor").as_ptr());
        }
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
            if ev == WM_RBUTTONUP || ev == WM_CONTEXTMENU { menu(hwnd); } else if ev == WM_LBUTTONDBLCLK { if !open_app() { flyout::show(dir()); } } else if ev == WM_LBUTTONUP { flyout::show(dir()); }
            0
        }
        WM_HOTKEY => { run_fix(&["quick"]); 0 }
        WM_TIMER => {
            let flag = dir().join("stop-tray");
            if flag.exists() { let _ = std::fs::remove_file(&flag); DestroyWindow(hwnd); } else { watchdog(); fixer_watchdog(); refresh(hwnd, false); }
            0
        }
        WM_COMMAND => {
            match (wp & 0xFFFF) as u32 {
                9 => { if !open_app() { open_report(); } }
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
                c @ 200..=299 => {
                    let cmds = &*std::ptr::addr_of!(GOV_CMDS);
                    if let Some(a) = cmds.get((c - 200) as usize) { let v: Vec<&str> = a.iter().map(|s| s.as_str()).collect(); run_fix(&v); }
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

pub fn main() {
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
        // Ctrl+Alt+Shift+S: quick fix, for when the mouse barely moves but the keyboard still works.
        windows_sys::Win32::UI::Input::KeyboardAndMouse::RegisterHotKey(hwnd, 1, 0x0002 | 0x0001 | 0x0004 | 0x4000, 'S' as u32);
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

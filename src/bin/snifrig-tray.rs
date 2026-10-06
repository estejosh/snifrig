// Snifrig tray icon. Reads status.json written by the monitor and owns no monitoring of its own,
// so its memory never counts against the monitor's budget. Windowless, one thread, a 15 s timer.
#![cfg_attr(windows, windows_subsystem = "windows")]
#[path = "../icon.rs"]
mod icon;

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

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
fn dir() -> &'static PathBuf { DIR.get().unwrap() }
fn put(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

struct Status { level: usize, avail_mb: u64, alerts: u64, last_alert_unix: u64, last_alert: String }

fn read_status() -> Status {
    let s = std::fs::read_to_string(dir().join("status.json")).unwrap_or_default();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let unix = snifrig::json_num(&s, "unix");
    let fresh = unix > 0 && now.saturating_sub(unix) < 180;
    let level = if !fresh { 2 } else if snifrig::json_str(&s, "level").as_deref() == Some("alert") { 1 } else { 0 };
    Status {
        level, avail_mb: snifrig::json_num(&s, "avail_mb"), alerts: snifrig::json_num(&s, "alerts_2h"),
        last_alert_unix: snifrig::json_num(&s, "last_alert_unix"), last_alert: snifrig::json_str(&s, "last_alert").unwrap_or_default(),
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
    put(&mut n.szTip, &tip);
    if !FIRST && st.level == 1 && st.last_alert_unix > LAST_ALERT {
        n.uFlags |= NIF_INFO;
        put(&mut n.szInfoTitle, "Snifrig alert");
        put(&mut n.szInfo, &st.last_alert);
        n.dwInfoFlags = NIIF_WARNING;
    }
    if st.last_alert_unix > LAST_ALERT { LAST_ALERT = st.last_alert_unix; }
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

unsafe fn menu(hwnd: HWND) {
    let st = read_status();
    let m = CreatePopupMenu();
    let head = format!("Snifrig: {}", ["OK", "ALERT", "monitor not running"][st.level]);
    AppendMenuW(m, MF_STRING | MF_GRAYED, 0, wide(&head).as_ptr());
    AppendMenuW(m, MF_SEPARATOR, 0, null());
    AppendMenuW(m, MF_STRING, 1, wide("Show report").as_ptr());
    AppendMenuW(m, MF_STRING, 2, wide("Open alerts").as_ptr());
    AppendMenuW(m, MF_STRING, 3, wide("Open data folder").as_ptr());
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
            if ev == WM_RBUTTONUP || ev == WM_CONTEXTMENU { menu(hwnd); } else if ev == WM_LBUTTONDBLCLK { open_report(); }
            0
        }
        WM_TIMER => {
            let flag = dir().join("stop-tray");
            if flag.exists() { let _ = std::fs::remove_file(&flag); DestroyWindow(hwnd); } else { refresh(hwnd, false); }
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
    let a: Vec<String> = std::env::args().collect();
    let d = a.iter().position(|x| x == "--dir").and_then(|i| a.get(i + 1)).map(PathBuf::from).unwrap_or_else(snifrig::default_dir);
    let _ = std::fs::create_dir_all(&d);
    DIR.set(d).ok();
    unsafe {
        let mn = wide("Global\\snifrig-tray");
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
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

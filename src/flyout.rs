// Tray flyout: left-click shows CPU / RAM / GPU / VRAM with the top consumers of each.
// Data comes from `snifrig --snapshot`, run on demand, so nothing is sampled while the flyout is closed.
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const BW: i32 = 380;
const BH: i32 = 318;static mut DPI: i32 = 96;
fn sc(n: i32) -> i32 { unsafe { n * *std::ptr::addr_of!(DPI) / 96 } }
static mut FLY: HWND = null_mut();
static mut SNAP: String = String::new();
static mut FDIR: Option<PathBuf> = None;
static mut REG: bool = false;

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
fn rgb(r: u32, g: u32, b: u32) -> u32 { r | (g << 8) | (b << 16) }

fn section<'a>(s: &'a str, key: &str) -> &'a str {
    let k = format!("\"{}\":{{", key);
    if let Some(i) = s.find(&k) {
        let st = i + k.len() - 1;
        let mut d = 0;
        for (j, c) in s[st..].char_indices() {
            if c == '{' { d += 1; } else if c == '}' { d -= 1; if d == 0 { return &s[st..st + j + 1]; } }
        }
    }
    ""
}
fn num(s: &str, key: &str) -> f64 {
    let k = format!("\"{}\":", key);
    s.find(&k).and_then(|i| {
        let r = &s[i + k.len()..];
        let e = r.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(r.len());
        r[..e].parse().ok()
    }).unwrap_or(0.0)
}
fn pairs(s: &str) -> Vec<(String, f64)> {
    let mut v = Vec::new();
    if let Some(i) = s.find("\"top\":[") {
        let r = &s[i + 7..];
        let end = r.find("]]").map(|x| x + 1).unwrap_or(r.len());
        for part in r[..end].split("],[") {
            let p = part.trim_matches(|c| c == '[' || c == ']');
            if let Some((n, x)) = p.rsplit_once(',') { v.push((n.trim_matches('"').to_string(), x.parse().unwrap_or(0.0))); }
        }
    }
    v
}

unsafe fn load() {
    if let Some(d) = (*std::ptr::addr_of!(FDIR)).as_ref() {
        *std::ptr::addr_of_mut!(SNAP) = std::fs::read_to_string(d.join("snapshot.json")).unwrap_or_default();
    }
}

pub unsafe fn show(dir: &Path) {
    if !FLY.is_null() { DestroyWindow(FLY); FLY = null_mut(); return; }
    *std::ptr::addr_of_mut!(FDIR) = Some(dir.to_path_buf());
    *std::ptr::addr_of_mut!(SNAP) = String::new();
    if let Some(exe) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("snifrig.exe"))) {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        let _ = std::process::Command::new(exe).arg("--snapshot").arg("--dir").arg(dir)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x0800_0000).spawn();
    }
    let hi = GetModuleHandleW(null());
    let cls = wide("SnifrigFlyout");
    if !REG {
        let wc = WNDCLASSW { style: 0, lpfnWndProc: Some(proc_), cbClsExtra: 0, cbWndExtra: 0, hInstance: hi, hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW), hbrBackground: null_mut(), lpszMenuName: null(), lpszClassName: cls.as_ptr() };
        RegisterClassW(&wc);
        REG = true;
    }
    { let sdc = GetDC(null_mut()); let d = GetDeviceCaps(sdc, LOGPIXELSX as i32); ReleaseDC(null_mut(), sdc); DPI = if d >= 96 { d } else { 96 }; }
    let mut wa: RECT = std::mem::zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as *mut _, 0);
    let (x, y) = (wa.right - sc(BW) - sc(12), wa.bottom - sc(BH) - sc(12));
    FLY = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, cls.as_ptr(), wide("Snifrig").as_ptr(), WS_POPUP | WS_VISIBLE | WS_BORDER,
        x, y, sc(BW), sc(BH), null_mut(), null_mut(), hi, null());
    SetForegroundWindow(FLY);
    SetTimer(FLY, 1, 1700, None);
}

unsafe fn text(dc: HDC, x: i32, y: i32, s: &str, col: u32, right: bool) {
    SetTextColor(dc, col);
    let w = wide(s);
    let mut r = RECT { left: x, top: y, right: sc(BW) - sc(20), bottom: y + sc(20) };
    DrawTextW(dc, w.as_ptr(), -1, &mut r, DT_SINGLELINE | DT_END_ELLIPSIS | if right { DT_RIGHT } else { DT_LEFT });
}

unsafe fn row(dc: HDC, y: i32, label: &str, val: &str, frac: f64, top: &[(String, f64)], unit: &str, big: HFONT, small: HFONT) {
    let old = SelectObject(dc, big);
    text(dc, sc(20), y, label, rgb(0xD9, 0xA4, 0x41), false);
    text(dc, sc(20), y, val, rgb(0xE7, 0xDC, 0xC0), true);
    SelectObject(dc, small);
    let (bx, by, bw) = (sc(20), y + sc(26), sc(BW) - sc(40));
    let track = CreateSolidBrush(rgb(0x2A, 0x32, 0x3A));
    FillRect(dc, &RECT { left: bx, top: by, right: bx + bw, bottom: by + sc(8) }, track);
    DeleteObject(track);
    if frac >= 0.0 {
        let col = if frac > 0.9 { rgb(0xE0, 0x5A, 0x4A) } else { rgb(0x3F, 0xB8, 0xE8) };
        let b = CreateSolidBrush(col);
        FillRect(dc, &RECT { left: bx, top: by, right: bx + ((bw as f64) * frac.min(1.0)) as i32, bottom: by + sc(8) }, b);
        DeleteObject(b);
    }
    let t = if top.is_empty() { "-".to_string() } else { top.iter().map(|(n, v)| format!("{}  {}{}", n, v, unit)).collect::<Vec<_>>().join("   ") };
    text(dc, sc(20), by + sc(14), &t, rgb(0x9A, 0xA5, 0xAE), false);
    SelectObject(dc, old);
}

unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    let bg = CreateSolidBrush(rgb(0x14, 0x18, 0x1C));
    FillRect(dc, &RECT { left: 0, top: 0, right: sc(BW), bottom: sc(BH) }, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as i32);
    let face = wide("Segoe UI");
    let big = CreateFontW(-sc(17), 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    let small = CreateFontW(-sc(13), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    let s = &*std::ptr::addr_of!(SNAP);
    let old = SelectObject(dc, big);
    text(dc, sc(20), sc(12), "SNIFRIG", rgb(0xD9, 0xA4, 0x41), false);
    SelectObject(dc, small);
    if s.is_empty() {
        text(dc, sc(20), sc(14), "sampling...", rgb(0x9A, 0xA5, 0xAE), true);
    } else {
        let (c, r, g, v) = (section(s, "cpu"), section(s, "ram"), section(s, "gpu"), section(s, "vram"));
        let cpu = num(c, "total");
        row(dc, sc(46), "CPU", &format!("{:.0}%", cpu), cpu / 100.0, &pairs(c), "%", big, small);
        let (ru, rt) = (num(r, "used_mb"), num(r, "total_mb").max(1.0));
        let rtop: Vec<(String, f64)> = pairs(r).into_iter().map(|(n, x)| (n, (x / 1024.0 * 10.0).round() / 10.0)).collect();
        row(dc, sc(116), "RAM", &format!("{:.1} / {:.0} GB", ru / 1024.0, rt / 1024.0), ru / rt, &rtop, " GB", big, small);
        if g.is_empty() {
            row(dc, sc(186), "GPU", "no counters", -1.0, &[], "", big, small);
        } else {
            let gu = num(g, "util");
            row(dc, sc(186), "GPU", &format!("{:.0}%", gu), gu / 100.0, &pairs(g), "%", big, small);
            let vtop: Vec<(String, f64)> = pairs(v).into_iter().map(|(n, x)| (n, (x / 1024.0 * 10.0).round() / 10.0)).collect();
            let (vu, vt) = (num(v, "used_mb"), num(v, "total_mb"));
            if vt > 0.0 {
                row(dc, sc(256), "VRAM", &format!("{:.1} / {:.0} GB", vu / 1024.0, vt / 1024.0), vu / vt, &vtop, " GB", big, small);
            } else {
                row(dc, sc(256), "VRAM", &format!("{:.1} GB used", vu / 1024.0), -1.0, &vtop, " GB", big, small);
            }
        }
    }
    SelectObject(dc, old);
    DeleteObject(big);
    DeleteObject(small);
    EndPaint(hwnd, &ps);
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => { paint(hwnd); 0 }
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            load();
            if !(&*std::ptr::addr_of!(SNAP)).is_empty() { KillTimer(hwnd, 1); }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_ACTIVATE => { if (wp & 0xFFFF) as u32 == WA_INACTIVE && std::env::var_os("SNIFRIG_STICKY").is_none() { DestroyWindow(hwnd); } 0 }
        WM_KEYDOWN => { if wp as u32 == 0x1B { DestroyWindow(hwnd); } 0 }
        WM_DESTROY => { FLY = null_mut(); 0 }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

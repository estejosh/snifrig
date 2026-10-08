// UFL Section 2D Notice. estejosh writes the final copy; change only these two consts.
pub const NOTICE_TITLE: &str = "Notice";
pub const NOTICE_TEXT: &str = "snifrig snif is free, snifrig fix is paid";

// The tray's Notice window: static text, no network, no tracking, no links, no sound.
// Never takes focus. Closes after 8 s (one timer, never restarted), on any click, or on Esc.
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[link(name = "user32")]
extern "system" { fn GetAsyncKeyState(vk: i32) -> i16; }

const NW: i32 = 340;
const NH: i32 = 90;
const LIFE_MS: u128 = 8000;
static mut DPI: i32 = 96;
static mut NOTE: HWND = null_mut();
static mut T0: Option<std::time::Instant> = None;
fn sc(n: i32) -> i32 { unsafe { n * *std::ptr::addr_of!(DPI) / 96 } }
fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
fn rgb(r: u32, g: u32, b: u32) -> u32 { r | (g << 8) | (b << 16) }

/// Show the Notice once. The caller decides when (tray start); this never activates the window.
pub unsafe fn show() {
    if !NOTE.is_null() { return; }
    let hi = GetModuleHandleW(null());
    let cls = wide("SnifrigNotice");
    let wc = WNDCLASSW { style: 0, lpfnWndProc: Some(proc_), cbClsExtra: 0, cbWndExtra: 0, hInstance: hi, hIcon: null_mut(),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW), hbrBackground: null_mut(), lpszMenuName: null(), lpszClassName: cls.as_ptr() };
    RegisterClassW(&wc);
    { let sdc = GetDC(null_mut()); let d = GetDeviceCaps(sdc, LOGPIXELSX as i32); ReleaseDC(null_mut(), sdc); DPI = if d >= 96 { d } else { 96 }; }
    let mut wa: RECT = std::mem::zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as *mut _, 0);
    let (x, y) = (wa.right - sc(NW) - sc(12), wa.bottom - sc(NH) - sc(12));
    NOTE = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, cls.as_ptr(), wide(NOTICE_TITLE).as_ptr(), WS_POPUP | WS_BORDER,
        x, y, sc(NW), sc(NH), null_mut(), null_mut(), hi, null());
    if NOTE.is_null() { return; }
    T0 = Some(std::time::Instant::now());
    SetTimer(NOTE, 1, 100, None); // one timer: polls Esc (a no-activate window gets no key events) and ends the 8 s life
    ShowWindow(NOTE, SW_SHOWNOACTIVATE);
}

unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    let bg = CreateSolidBrush(rgb(0x22, 0x25, 0x2A));
    FillRect(dc, &RECT { left: 0, top: 0, right: sc(NW), bottom: sc(NH) }, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as i32);
    let face = wide("Segoe UI");
    let small = CreateFontW(-sc(12), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    let body = CreateFontW(-sc(15), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    let old = SelectObject(dc, small);
    SetTextColor(dc, rgb(0x8A, 0x8F, 0x98));
    let t = wide(NOTICE_TITLE);
    let mut r = RECT { left: sc(14), top: sc(8), right: sc(NW) - sc(40), bottom: sc(26) };
    DrawTextW(dc, t.as_ptr(), -1, &mut r, DT_SINGLELINE | DT_LEFT);
    let x = wide("\u{00D7}");
    let mut r = RECT { left: sc(NW) - sc(34), top: sc(6), right: sc(NW) - sc(12), bottom: sc(26) };
    DrawTextW(dc, x.as_ptr(), -1, &mut r, DT_SINGLELINE | DT_RIGHT);
    SelectObject(dc, body);
    SetTextColor(dc, rgb(0xD8, 0xDB, 0xE0));
    let m = wide(NOTICE_TEXT);
    let mut r = RECT { left: sc(14), top: sc(30), right: sc(NW) - sc(14), bottom: sc(NH) - sc(8) };
    DrawTextW(dc, m.as_ptr(), -1, &mut r, DT_WORDBREAK | DT_LEFT);
    SelectObject(dc, old);
    DeleteObject(small);
    DeleteObject(body);
    EndPaint(hwnd, &ps);
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => { paint(hwnd); 0 }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => { DestroyWindow(hwnd); 0 }
        WM_TIMER => {
            let aged = (*std::ptr::addr_of!(T0)).map(|t| t.elapsed().as_millis() >= LIFE_MS).unwrap_or(true);
            if aged || (GetAsyncKeyState(0x1B) as u16 & 0x8000) != 0 { DestroyWindow(hwnd); }
            0
        }
        WM_DESTROY => { KillTimer(hwnd, 1); 0 }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

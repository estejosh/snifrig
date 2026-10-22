//! Pop-up guard: notices when a window takes the screen without you asking for it, and names
//! who did it. Once you say yes, it stops: a scheduled task becomes a silent snifrig job, and an
//! app that grabs focus is put back behind what you were doing the moment it does.

#[link(name = "user32", kind = "raw-dylib")]
extern "system" {
    fn GetForegroundWindow() -> isize;
    fn GetWindowThreadProcessId(h: isize, pid: *mut u32) -> u32;
    fn GetWindowTextW(h: isize, s: *mut u16, n: i32) -> i32;
    fn GetClassNameW(h: isize, s: *mut u16, n: i32) -> i32;
    fn GetCursorPos(p: *mut [i32; 2]) -> i32;
    fn GetWindowRect(h: isize, r: *mut [i32; 4]) -> i32;
    fn GetAsyncKeyState(vk: i32) -> i16;
    fn AttachThreadInput(a: u32, b: u32, attach: i32) -> i32;
    fn SetForegroundWindow(h: isize) -> i32;
    fn SetWindowPos(h: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn IsWindow(h: isize) -> i32;
}
#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" { fn GetCurrentThreadId() -> u32; }

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fg { pub hwnd: isize, pub pid: u32, pub title: String, pub class: String }

fn text(f: unsafe extern "system" fn(isize, *mut u16, i32) -> i32, h: isize) -> String {
    let mut b = [0u16; 256];
    let n = unsafe { f(h, b.as_mut_ptr(), 256) }.max(0) as usize;
    String::from_utf16_lossy(&b[..n])
}

/// The window in front right now.
pub fn foreground() -> Option<Fg> {
    let h = unsafe { GetForegroundWindow() };
    if h == 0 { return None; }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, &mut pid); }
    Some(Fg { hwnd: h, pid, title: text(GetWindowTextW, h), class: text(GetClassNameW, h) })
}

/// Pure: did you bring this window forward yourself? A key held for switching (Alt, Win), or a
/// click or key just now with the mouse over the new window, counts as you.
pub fn user_caused(idle_ms: f64, switch_key_down: bool, cursor_over_new: bool) -> bool {
    switch_key_down || idle_ms < 150.0 || (idle_ms < 500.0 && cursor_over_new)
}

pub fn switch_key_down() -> bool {
    [0x12, 0x5B, 0x5C, 0x11].iter().any(|vk| unsafe { GetAsyncKeyState(*vk) } as u16 & 0x8000 != 0)
}

pub fn cursor_over(h: isize) -> bool {
    let (mut p, mut r) = ([0i32; 2], [0i32; 4]);
    unsafe { GetCursorPos(&mut p) != 0 && GetWindowRect(h, &mut r) != 0 && p[0] >= r[0] && p[0] < r[2] && p[1] >= r[1] && p[1] < r[3] }
}

/// Windows' own screens and snifrig: never counted as stealing focus.
pub const SYSTEM: &[&str] = &[
    "dwm.exe", "explorer.exe", "consent.exe", "logonui.exe", "lockapp.exe", "searchhost.exe", "startmenuexperiencehost.exe",
    "shellexperiencehost.exe", "shellhost.exe", "textinputhost.exe", "applicationframehost.exe", "taskmgr.exe", "sihost.exe",
];

pub fn is_system(name: &str) -> bool { let n = name.to_lowercase(); n.starts_with("snifrig") || SYSTEM.contains(&n.as_str()) }

/// Console windows: the app that "stole" focus is the console host; the real culprit is the
/// program that opened the console.
pub fn is_console_host(name: &str) -> bool { matches!(name.to_lowercase().as_str(), "windowsterminal.exe" | "openconsole.exe" | "conhost.exe") }

/// Give the screen back to `prev` and push the window that took it behind everything,
/// without activating it.
pub fn give_back(prev: isize, thief: isize) -> bool {
    unsafe {
        if IsWindow(prev) == 0 { return false; }
        let me = GetCurrentThreadId();
        let them = GetWindowThreadProcessId(thief, std::ptr::null_mut());
        let attached = them != 0 && AttachThreadInput(me, them, 1) != 0;
        let ok = SetForegroundWindow(prev) != 0;
        if attached { AttachThreadInput(me, them, 0); }
        SetWindowPos(thief, 1 /* HWND_BOTTOM */, 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0010 /* NOSIZE NOMOVE NOACTIVATE */);
        ok
    }
}

/// What took the screen, in plain words, and what "make it stop" means for it.
#[derive(Clone, Debug, PartialEq)]
pub enum Culprit {
    /// A Windows scheduled task (full task path), and the program it ran.
    Task(String, String),
    /// An app (image name) that opened a window over yours.
    App(String),
}

impl Culprit {
    pub fn key(&self) -> String { match self { Culprit::Task(t, _) => t.clone(), Culprit::App(a) => a.to_lowercase() } }
    pub fn says(&self) -> String {
        match self {
            Culprit::Task(t, p) => format!("The scheduled task {} opened a window ({})", t.trim_start_matches('\\'), p),
            Culprit::App(a) => format!("{} opened a window", a),
        }
    }
}

/// Pure: in a process chain (newest first: name, command line), is there a Task Scheduler
/// launch, and which program in the chain did it start?
pub fn task_launch(chain: &[(String, String)]) -> Option<String> {
    for i in 0..chain.len() {
        let (n, c) = (&chain[i].0.to_lowercase(), chain[i].1.to_lowercase());
        let sched = n == "taskhostw.exe" || n == "taskeng.exe" || (n == "svchost.exe" && c.contains("schedule"));
        if sched && i > 0 { return Some(chain[i - 1].1.clone()); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn you_or_a_popup() {
        assert!(user_caused(30.0, false, false), "a click or key a moment ago");
        assert!(user_caused(300.0, false, true), "you clicked the window that came forward");
        assert!(user_caused(5000.0, true, false), "alt-tab");
        assert!(!user_caused(1500.0, false, false), "nothing from you: it took the screen by itself");
        assert!(!user_caused(300.0, false, false), "you were clicking somewhere else");
    }

    #[test]
    fn finds_the_task_that_started_a_console() {
        let chain = vec![
            ("powershell.exe".to_string(), "powershell.exe -File X:\\w.ps1".to_string()),
            ("svchost.exe".to_string(), "C:\\Windows\\system32\\svchost.exe -k netsvcs -p -s Schedule".to_string()),
            ("services.exe".to_string(), String::new()),
        ];
        assert_eq!(task_launch(&chain), Some("powershell.exe -File X:\\w.ps1".to_string()));
        let app = vec![("python.exe".to_string(), "python x.py".to_string()), ("code.exe".to_string(), "code".to_string())];
        assert_eq!(task_launch(&app), None);
    }

    #[test]
    fn system_screens_are_not_popups() {
        assert!(is_system("consent.exe"));
        assert!(is_system("snifrig-app.exe"));
        assert!(!is_system("zoom.exe"));
        assert!(is_console_host("WindowsTerminal.exe"));
    }
}

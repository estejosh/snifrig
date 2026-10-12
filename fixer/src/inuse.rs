//! Is the user using this app? If so, snifrig reins it in (trim, efficiency, priority) and
//! never ends or restarts it. Signals, all cheap:
//! - it has a visible, titled window (multi-process apps: any process of the same image name),
//! - it was the app in front in the last 30 minutes,
//! - the local brain says it is an interactive app, a game, or an AI model server.
//! The watch loop feeds foreground and brain data in; anything in this process can ask.
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

pub const RECENT_SECS: f64 = 30.0 * 60.0;
const USED_KINDS: [&str; 3] = ["interactive", "game", "ai_model"];

#[derive(Default)]
struct State { last_fg: HashMap<String, f64>, kinds: HashMap<String, String> }

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(State::default))
}

/// The app in front right now (lowercase image name).
pub fn note_foreground(name: &str, now: f64) { with(|s| { s.last_fg.insert(name.to_lowercase(), now); }); }

/// Brain classifications above its confidence floor: lowercase name -> kind.
pub fn set_kinds(kinds: HashMap<String, String>) { with(|s| s.kinds = kinds); }

/// Pure decision, so it can be tested: why the app counts as in use, or None.
pub fn reason(name: &str, now: f64, windowed_names: &HashSet<String>, last_fg: Option<f64>, kind: Option<&str>) -> Option<String> {
    let n = name.to_lowercase();
    if windowed_names.contains(&n) { return Some("it has a window open".into()); }
    if let Some(t) = last_fg { if now - t < RECENT_SECS { return Some(format!("you used it {} minutes ago", ((now - t) / 60.0).round().max(1.0))); } }
    if let Some(k) = kind { if USED_KINDS.contains(&k) { return Some(match k { "game" => "it is a game".into(), "ai_model" => "it is an AI model you run".into(), _ => "it is an app you use".into() }); } }
    None
}

/// Why `name` counts as in use right now, or None. Reads windows live.
pub fn why_in_use(name: &str, now: f64) -> Option<String> {
    let windowed = windowed_names();
    let (fg, kind) = with(|s| (s.last_fg.get(&name.to_lowercase()).copied(), s.kinds.get(&name.to_lowercase()).cloned()));
    reason(name, now, &windowed, fg, kind.as_deref())
}

/// Lowercase image names that own a visible, titled top-level window.
pub fn windowed_names() -> HashSet<String> {
    let pids = crate::sys::window_pids();
    let mut buf = Vec::new();
    crate::sys::snapshot(&mut buf).into_iter().filter(|p| pids.contains(&p.pid)).map(|p| p.name.to_lowercase()).collect()
}

#[cfg(test)]
mod inuse_tests {
    use super::*;
    #[test]
    fn signals() {
        let w: HashSet<String> = ["comet.exe".to_string()].into();
        assert!(reason("Comet.exe", 1000.0, &w, None, None).is_some());
        assert!(reason("claude.exe", 5000.0, &HashSet::new(), Some(4000.0), None).unwrap().contains("17 minutes"));
        assert!(reason("claude.exe", 5000.0, &HashSet::new(), Some(1000.0), None).is_none());
        assert_eq!(reason("llama-server.exe", 0.0, &HashSet::new(), None, Some("ai_model")).as_deref(), Some("it is an AI model you run"));
        assert!(reason("ffmpeg.exe", 0.0, &HashSet::new(), None, Some("media_batch")).is_none());
    }
}

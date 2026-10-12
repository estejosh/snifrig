//! snifrig app: the window. It shows what snifrig sees and lets you change everything the
//! fixer does without touching a text file. It only runs while open; closing it ends it.
#![windows_subsystem = "windows"]
#![allow(dead_code)]

mod data;
mod json;
mod sys;

use data::{level_name, Rule, State, Tuning};
use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, RichText, Stroke, TextStyle, Vec2};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BG: Color32 = Color32::from_rgb(17, 19, 23);
const SIDE: Color32 = Color32::from_rgb(22, 25, 30);
const CARD: Color32 = Color32::from_rgb(28, 32, 38);
const CARD2: Color32 = Color32::from_rgb(35, 40, 47);
const BORDER: Color32 = Color32::from_rgb(44, 50, 58);
const TEXT: Color32 = Color32::from_rgb(228, 231, 235);
const MUTED: Color32 = Color32::from_rgb(138, 147, 158);
const ACCENT: Color32 = Color32::from_rgb(232, 163, 61);
const GOOD: Color32 = Color32::from_rgb(76, 195, 138);
const WARN: Color32 = Color32::from_rgb(230, 190, 80);
const BAD: Color32 = Color32::from_rgb(229, 83, 75);
const BLUE: Color32 = Color32::from_rgb(96, 165, 250);

#[derive(PartialEq, Clone, Copy)]
enum Page { Overview, Governor, Rules, Performance, Activity, Report, Settings }

#[derive(PartialEq, Clone, Copy)]
enum Filter { All, Governor, Rules, Fixes, Performance, Learning }

type Toast = Arc<Mutex<Option<(String, bool, Instant)>>>;

struct App {
    dir: PathBuf,
    live: Arc<Mutex<sys::Live>>,
    state: State,
    loaded: Instant,
    hist_loaded: Instant,
    page: Page,
    rules: data::RulesFile,
    edit: Option<(Option<usize>, Rule)>,
    pick: String,
    tuning: Tuning,
    tuning_saved: Tuning,
    plans: Vec<(String, String, bool)>,
    toast: Toast,
    report_days: u32,
    report: Arc<Mutex<Option<String>>>,
    filter: Filter,
    search: String,
    license: Arc<Mutex<String>>,
    brain_up: Option<bool>,
    logo: Option<egui::TextureHandle>,
    confirm_delete: Option<usize>,
    /// Compact "needs your OK" popup instead of the full window.
    ask: bool,
    snoozed: Vec<String>,
    sugg: Vec<data::Suggestion>,
}

fn main() -> eframe::Result<()> {
    let ask = std::env::args().any(|a| a == "--ask");
    if already_running(if ask { "Local\\snifrig-ask" } else { "Local\\snifrig-app" }, if ask { "snifrig needs your OK" } else { "snifrig" }) { return Ok(()); }
    let dir = std::env::args().skip_while(|a| a != "--dir").nth(1).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("snifrig"));
    let mut vp = if ask {
        let (_, _, r, b) = sys::work_area();
        let (w, h) = (460.0, 440.0);
        egui::ViewportBuilder::default().with_title("snifrig needs your OK").with_inner_size([w, h]).with_position([r - w - 16.0, b - h - 16.0])
            .with_decorations(false).with_always_on_top().with_resizable(false).with_taskbar(false)
    } else {
        egui::ViewportBuilder::default().with_title("snifrig").with_inner_size([1280.0, 840.0]).with_min_inner_size([900.0, 560.0])
    };
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-64.png")) { vp = vp.with_icon(Arc::new(icon)); }
    let opts = eframe::NativeOptions { viewport: vp, ..Default::default() };
    eframe::run_native("snifrig", opts, Box::new(move |cc| Ok(Box::new(App::new(cc, dir, ask)))))
}

fn already_running(mutex: &str, title: &str) -> bool {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};
    let name: Vec<u16> = mutex.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let _m = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if GetLastError() != 183 { return false; }
        let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let h = FindWindowW(std::ptr::null(), title.as_ptr());
        if !h.is_null() { ShowWindow(h, SW_RESTORE); SetForegroundWindow(h); }
        true
    }
}

fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, path) in [("segoe", "C:\\Windows\\Fonts\\segoeui.ttf"), ("segoe-bold", "C:\\Windows\\Fonts\\segoeuib.ttf")] {
        if let Ok(b) = std::fs::read(path) { fonts.font_data.insert(name.into(), egui::FontData::from_owned(b).into()); }
    }
    if fonts.font_data.contains_key("segoe") {
        fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "segoe".into());
    }
    if fonts.font_data.contains_key("segoe-bold") {
        let mut v = vec!["segoe-bold".to_string()];
        v.extend(fonts.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default());
        fonts.families.insert(egui::FontFamily::Name("bold".into()), v);
    } else {
        let v = fonts.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default();
        fonts.families.insert(egui::FontFamily::Name("bold".into()), v);
    }
    ctx.set_fonts(fonts);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(13, 15, 18);
    v.faint_bg_color = CARD2;
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = Color32::from_rgb(120, 84, 30);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.widgets.inactive.weak_bg_fill = CARD2;
    v.widgets.inactive.bg_fill = CARD2;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(48, 54, 63);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(60, 66, 76);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.slider_trailing_fill = true;
    ctx.set_visuals(v);
    let mut s = (*ctx.style()).clone();
    s.spacing.item_spacing = Vec2::new(8.0, 8.0);
    s.spacing.button_padding = Vec2::new(12.0, 6.0);
    s.spacing.slider_width = 260.0;
    s.text_styles.insert(TextStyle::Body, FontId::proportional(14.5));
    s.text_styles.insert(TextStyle::Button, FontId::proportional(14.0));
    s.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
    s.text_styles.insert(TextStyle::Heading, FontId::new(24.0, egui::FontFamily::Name("bold".into())));
    s.text_styles.insert(TextStyle::Monospace, FontId::monospace(13.0));
    ctx.set_style(s);
}

fn bold(t: impl Into<String>, size: f32) -> RichText { RichText::new(t).font(FontId::new(size, egui::FontFamily::Name("bold".into()))) }

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new().fill(CARD).stroke(Stroke::new(1.0, BORDER)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(16))
        .show(ui, |ui| { ui.set_width(ui.available_width()); add(ui) }).inner
}

fn chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new().fill(color.gamma_multiply(0.18)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| { ui.label(RichText::new(text).size(12.5).color(color)); });
}

fn section(ui: &mut egui::Ui, title: &str, sub: &str) {
    ui.label(bold(title, 17.0).color(TEXT));
    if !sub.is_empty() { ui.label(RichText::new(sub).color(MUTED)); }
    ui.add_space(4.0);
}

/// A setting row: label above, slider using the width the card has.
fn srow<N: egui::emath::Numeric>(ui: &mut egui::Ui, label: &str, v: &mut N, range: std::ops::RangeInclusive<N>, suffix: &str) {
    ui.label(RichText::new(label).color(MUTED));
    ui.spacing_mut().slider_width = (ui.available_width() - 120.0).max(80.0);
    ui.add(egui::Slider::new(v, range).suffix(suffix));
    ui.add_space(2.0);
}

fn sparkline(ui: &mut egui::Ui, data: &[f32], max: f32, color: Color32, height: f32, marks: &[(f32, Color32)]) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, height), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(6), Color32::from_rgb(20, 23, 28));
    let y = |v: f32| rect.bottom() - (v / max).clamp(0.0, 1.0) * (rect.height() - 4.0) - 2.0;
    for (m, c) in marks {
        p.line_segment([egui::pos2(rect.left(), y(*m)), egui::pos2(rect.right(), y(*m))], Stroke::new(1.0, c.gamma_multiply(0.6)));
    }
    if data.len() < 2 { return; }
    let n = data.len().max(2) as f32 - 1.0;
    let pts: Vec<egui::Pos2> = data.iter().enumerate().map(|(i, v)| egui::pos2(rect.left() + i as f32 / n * rect.width(), y(*v))).collect();
    let mut mesh = egui::Mesh::default();
    let fill = color.gamma_multiply(0.18);
    for (i, pt) in pts.iter().enumerate() {
        mesh.colored_vertex(*pt, fill);
        mesh.colored_vertex(egui::pos2(pt.x, rect.bottom()), Color32::TRANSPARENT);
        if i > 0 { let k = (i * 2) as u32; mesh.add_triangle(k - 2, k - 1, k); mesh.add_triangle(k - 1, k + 1, k); }
    }
    p.add(egui::Shape::mesh(mesh));
    p.add(egui::Shape::line(pts, Stroke::new(2.0, color)));
}

fn bar(ui: &mut egui::Ui, frac: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(4), Color32::from_rgb(20, 23, 28));
    let mut r = rect;
    r.set_width(rect.width() * frac.clamp(0.0, 1.0));
    p.rect_filled(r, CornerRadius::same(4), color);
}

fn local_time(unix: f64, with_date: bool) -> String {
    let t = unix as i64 + sys::local_offset_secs();
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if !with_date { return format!("{:02}:{:02}", h, m); }
    // civil from days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} {:02}:{:02}", M[(mo - 1) as usize], d, h, m)
}

fn mode_label(m: &str) -> &'static str { match m { "off" => "Off", "dry-run" => "Watch only", "ask" => "Ask me first", "auto" => "Automatic", _ => "Watch only" } }

fn action_label(a: &str) -> &'static str {
    match a { "terminate" => "End it", "restart-service" => "Restart its service", "trim" => "Free its memory", "lower-priority" => "Lower its priority", _ => "Look at it" }
}

impl App {
    fn new(cc: &eframe::CreationContext, dir: PathBuf, ask: bool) -> App {
        setup_style(&cc.egui_ctx);
        let logo = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-128.png")).ok().map(|i| {
            let img = egui::ColorImage::from_rgba_unmultiplied([i.width as usize, i.height as usize], &i.rgba);
            cc.egui_ctx.load_texture("logo", img, egui::TextureOptions::LINEAR)
        });
        let tuning = data::load_tuning(&dir);
        let state = data::load_state(&dir, true, &State::default());
        let app = App {
            rules: data::load_rules(&dir), edit: None, pick: String::new(), tuning_saved: tuning.clone(), tuning,
            plans: Vec::new(), toast: Arc::new(Mutex::new(None)), report_days: 7, report: Arc::new(Mutex::new(None)),
            filter: Filter::All, search: String::new(), license: Arc::new(Mutex::new(String::new())), brain_up: None,
            ask, snoozed: Vec::new(), sugg: data::load_suggestions(&dir),
            live: sys::start(), state, loaded: Instant::now(), hist_loaded: Instant::now(), page: Page::Overview, logo, confirm_delete: None, dir,
        };
        app.refresh_license();
        app
    }

    fn say(&self, msg: impl Into<String>, ok: bool) { *self.toast.lock().unwrap() = Some((msg.into(), ok, Instant::now())); }

    /// Run one of snifrig's own tools without a console window, off the UI thread.
    fn run(&self, exe: &str, args: Vec<String>, done: &str) {
        let (path, toast, done) = (data::tool(exe, &self.dir), self.toast.clone(), done.to_string());
        let dir = self.dir.clone();
        std::thread::spawn(move || {
            let mut a: Vec<String> = args;
            a.push("--dir".into());
            a.push(dir.to_string_lossy().into());
            let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
            let r = data::cmd(&path.to_string_lossy(), &refs);
            let msg = match &r { Ok(_) => (done, true), Err(e) => (format!("That didn't work: {}", e.lines().next().unwrap_or("")), false) };
            *toast.lock().unwrap() = Some((msg.0, msg.1, Instant::now()));
        });
    }

    fn refresh_license(&self) {
        let (path, out, dir) = (data::tool("snifrig-fix.exe", &self.dir), self.license.clone(), self.dir.clone());
        std::thread::spawn(move || {
            let r = if path.exists() { data::cmd(&path.to_string_lossy(), &["status", "--dir", &dir.to_string_lossy()]).unwrap_or_else(|e| e) } else { "snifrig fix is not installed.".into() };
            *out.lock().unwrap() = r;
        });
    }

    fn reload(&mut self) {
        let hist = self.hist_loaded.elapsed() > Duration::from_secs(60);
        if hist { self.hist_loaded = Instant::now(); }
        self.state = data::load_state(&self.dir, hist, &self.state);
        if self.edit.is_none() { self.rules = data::load_rules(&self.dir); }
        self.sugg = data::load_suggestions(&self.dir);
        if self.tuning == self.tuning_saved { self.tuning = data::load_tuning(&self.dir); self.tuning_saved = self.tuning.clone(); }
        self.loaded = Instant::now();
    }

    fn save_rules(&mut self, msg: &str) {
        match data::save_rules(&self.dir, &self.rules) { Ok(()) => self.say(msg, true), Err(e) => self.say(format!("Could not save rules: {}", e), false) }
    }

    fn save_tuning(&mut self) {
        match data::save_tuning(&self.dir, &self.tuning) {
            Ok(()) => { self.tuning_saved = self.tuning.clone(); self.say("Saved. snifrig fix picks this up within 30 seconds.", true) }
            Err(e) => self.say(format!("Could not save: {}", e), false),
        }
    }

    fn add_rule(&mut self, r: Rule, msg: &str) {
        if let Some(i) = self.rules.rules.iter().position(|x| x.pattern == r.pattern) { self.rules.rules[i] = r; } else { self.rules.rules.push(r); }
        self.save_rules(msg);
    }

    fn accept_suggestion(&mut self, s: &data::Suggestion) {
        let r = match s.kind.as_str() {
            "rule" => match Rule::parse(&s.rule) { Some(r) => { let p = r.pattern.clone(); self.add_rule(r, &format!("Rule for {} saved.", p)); Ok(()) } None => Err("the suggested rule could not be read".to_string()) },
            "ease" => { self.add_rule(Rule { pattern: s.app.to_lowercase(), priority: "below_normal".into(), efficiency: "on".into(), io: "low".into(), ..Default::default() }, &format!("{} will always run in the background.", s.app)); Ok(()) }
            "setting" => data::apply_setting(&self.dir, &s.setting).map(|_| { self.tuning = data::load_tuning(&self.dir); self.tuning_saved = self.tuning.clone(); self.say("Setting saved.", true); }),
            _ => Ok(()),
        };
        if let Err(e) = r { self.say(format!("Could not apply it: {}", e), false); }
        let _ = data::drop_suggestion(&self.dir, &s.id);
        self.sugg.retain(|x| x.id != s.id);
    }

    fn dismiss_suggestion(&mut self, s: &data::Suggestion) {
        let _ = data::drop_suggestion(&self.dir, &s.id);
        self.sugg.retain(|x| x.id != s.id);
    }

    /// One pending fix as a card with real buttons. Returns true if it was handled here.
    fn pending_card(&mut self, ui: &mut egui::Ui, p: &data::Pending, snooze: bool) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(bold(&p.name, 15.0).color(TEXT));
                chip(ui, action_label(&p.action), if p.action == "terminate" { BAD } else { BLUE });
            });
            ui.label(RichText::new(if p.why.is_empty() { &p.msg } else { &p.why }).size(12.5).color(MUTED));
            ui.horizontal(|ui| {
                let ends = p.action == "terminate" || p.action.starts_with("restart");
                if ends {
                    if ui.add(egui::Button::new(RichText::new("Rein it in").color(Color32::BLACK)).fill(ACCENT)).on_hover_text("Keep it running, but slower and with less memory").clicked() {
                        self.run("snifrig-fix.exe", vec!["ease".into(), p.id.clone()], &format!("{} is reined in: efficiency mode, lower priority, memory trimmed.", p.name));
                        self.snoozed.push(p.id.clone());
                    }
                    if ui.button(action_label(&p.action)).clicked() {
                        self.run("snifrig-fix.exe", vec!["approve".into(), p.id.clone()], &format!("Done: {} {}.", action_label(&p.action).to_lowercase(), p.name));
                        self.snoozed.push(p.id.clone());
                    }
                } else if ui.add(egui::Button::new(RichText::new("Approve").color(Color32::BLACK)).fill(ACCENT)).clicked() {
                    self.run("snifrig-fix.exe", vec!["approve".into(), p.id.clone()], &format!("Done: {} {}.", action_label(&p.action).to_lowercase(), p.name));
                    self.snoozed.push(p.id.clone());
                }
                if ui.button("Dismiss").clicked() { self.run("snifrig-fix.exe", vec!["dismiss".into(), p.id.clone()], "Dismissed."); self.snoozed.push(p.id.clone()); }
                if snooze && ui.button("Not now").clicked() { self.snoozed.push(p.id.clone()); }
            });
        });
    }

    fn suggestion_card(&mut self, ui: &mut egui::Ui, s: &data::Suggestion, snooze: bool) {
        card(ui, |ui| {
            ui.horizontal(|ui| { chip(ui, "suggested by your AI assistant", BLUE); });
            ui.label(bold(s.summary(), 15.0).color(TEXT));
            ui.label(RichText::new(&s.reason).size(12.5).color(MUTED));
            ui.horizontal(|ui| {
                if s.kind != "note" && ui.add(egui::Button::new(RichText::new("Accept").color(Color32::BLACK)).fill(ACCENT)).clicked() { self.accept_suggestion(s); }
                if ui.button(if s.kind == "note" { "Got it" } else { "Dismiss" }).clicked() { self.dismiss_suggestion(s); }
                if snooze && ui.button("Not now").clicked() { self.snoozed.push(s.id.clone()); }
            });
        });
    }

    fn ask_view(&mut self, ctx: &egui::Context) {
        let pending: Vec<data::Pending> = self.state.pending.iter().filter(|p| !self.snoozed.contains(&p.id)).cloned().collect();
        let sugg: Vec<data::Suggestion> = self.sugg.iter().filter(|s| !self.snoozed.contains(&s.id)).cloned().collect();
        let busy = self.toast.lock().unwrap().as_ref().map_or(false, |t| t.2.elapsed() < Duration::from_secs(3));
        if pending.is_empty() && sugg.is_empty() && !busy { ctx.send_viewport_cmd(egui::ViewportCommand::Close); return; }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).stroke(Stroke::new(1.0, ACCENT)).inner_margin(Margin::same(14))).show(ctx, |ui| {
            let head = ui.horizontal(|ui| {
                if let Some(t) = &self.logo { ui.add(egui::Image::new(t).fit_to_exact_size(Vec2::splat(26.0))); }
                ui.label(bold("snifrig needs your OK", 16.0).color(TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("\u{d7}").on_hover_text("Close; ask again later").clicked() { ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
                    if ui.small_button("Open snifrig").clicked() {
                        let _ = std::process::Command::new(std::env::current_exe().unwrap_or_default()).arg("--dir").arg(&self.dir).spawn();
                    }
                });
            }).response;
            if ui.interact(head.rect, egui::Id::new("drag"), egui::Sense::drag()).drag_started() { ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag); }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 34.0).show(ui, |ui| {
                for p in pending.iter().take(10) { self.pending_card(ui, p, true); ui.add_space(4.0); }
                for s in sugg.iter().take(10) { self.suggestion_card(ui, s, true); ui.add_space(4.0); }
            });
            let toast = self.toast.lock().unwrap().clone();
            if let Some((msg, ok, at)) = toast { if at.elapsed() < Duration::from_secs(6) { ui.label(RichText::new(msg).size(12.5).color(if ok { GOOD } else { BAD })); } }
        });
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn fixer_note(&self, ui: &mut egui::Ui) {
        if !self.state.has_key {
            card(ui, |ui| { ui.label(RichText::new("These settings take effect with snifrig fix, the paid part of snifrig. The monitor stays free.").color(WARN)); });
            ui.add_space(6.0);
        } else if !self.state.fixer_live {
            card(ui, |ui| { ui.label(RichText::new("snifrig fix is not running right now, so changes here wait until it starts.").color(WARN)); });
            ui.add_space(6.0);
        }
    }

    // ---------------- top bar and navigation ----------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(44.0);
            let (text, color) = if !self.state.monitor_live { ("The snifrig monitor is not running".to_string(), WARN) }
                else if self.state.headline.is_empty() || self.state.level == "ok" { ("Your PC is running smoothly".to_string(), GOOD) }
                else { (self.state.headline.clone(), if self.state.level == "alert" { WARN } else { BLUE }) };
            let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 5.0, color);
            ui.label(bold(text, 16.0).color(TEXT));
            if let Some(g) = &self.state.game { chip(ui, &format!("Performance mode: {}", g), ACCENT); }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let paused = self.state.paused_until > data::now();
                if paused {
                    if ui.button("Resume fixing").clicked() { self.run("snifrig.exe", vec!["resume".into()], "Fixing resumed."); }
                    ui.label(RichText::new(format!("Paused until {}", local_time(self.state.paused_until, false))).color(WARN));
                } else if ui.button("Pause 1 hour").on_hover_text("Stop all fixing for an hour. Monitoring continues.").clicked() {
                    self.run("snifrig.exe", vec!["pause".into(), "60".into()], "Fixing paused for an hour.");
                }
                if self.state.has_key {
                    let mut m = self.state.mode.clone();
                    egui::ComboBox::from_id_salt("mode").selected_text(mode_label(&m)).width(140.0).show_ui(ui, |ui| {
                        for k in ["off", "dry-run", "ask", "auto"] { ui.selectable_value(&mut m, k.to_string(), mode_label(k)); }
                    });
                    if m != self.state.mode {
                        self.state.mode = m.clone();
                        self.run("snifrig-fix.exe", vec!["mode".into(), m.clone()], &format!("Fixer set to {}.", mode_label(&m)));
                    }
                    ui.label(RichText::new("Fixer").color(MUTED));
                }
            });
        });
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if let Some(t) = &self.logo { ui.add(egui::Image::new(t).fit_to_exact_size(Vec2::splat(34.0))); }
            ui.vertical(|ui| {
                ui.label(bold("snifrig", 20.0).color(TEXT));
                ui.label(RichText::new("keeps your PC fast").size(12.0).color(MUTED));
            });
        });
        ui.add_space(18.0);
        let pend = self.state.pending.len();
        let items = [
            (Page::Overview, "\u{1F3E0}  Overview".to_string()),
            (Page::Governor, "\u{26A1}  Governor".to_string()),
            (Page::Rules, format!("\u{1F4CB}  App rules  {}", if self.rules.rules.is_empty() { String::new() } else { format!("({})", self.rules.rules.len()) })),
            (Page::Performance, "\u{1F3AE}  Performance mode".to_string()),
            (Page::Activity, "\u{1F4DC}  Activity".to_string()),
            (Page::Report, "\u{1F4CA}  Report".to_string()),
            (Page::Settings, format!("\u{2699}  Settings{}", if pend > 0 { String::new() } else { String::new() })),
        ];
        for (p, label) in items {
            let sel = self.page == p;
            let text = RichText::new(label).size(15.0).color(if sel { TEXT } else { MUTED });
            let b = egui::Button::new(text).fill(if sel { CARD2 } else { Color32::TRANSPARENT }).stroke(Stroke::NONE).min_size(Vec2::new(ui.available_width(), 36.0));
            if ui.add(b).clicked() { self.page = p; if p == Page::Settings { self.brain_up = None; self.refresh_license(); } }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(12.0);
            let dot = |ui: &mut egui::Ui, on: bool, text: &str| {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, if on { GOOD } else { MUTED });
                    ui.label(RichText::new(text).size(12.5).color(MUTED));
                });
            };
            let known = self.state.brain.len();
            dot(ui, known > 0, &format!("Local brain: knows {} apps", known));
            dot(ui, self.state.fixer_live, if self.state.fixer_live { "Fixer running" } else if self.state.has_key { "Fixer stopped" } else { "Fixer not licensed" });
            dot(ui, self.state.monitor_live, if self.state.monitor_live { "Monitor running" } else { "Monitor stopped" });
        });
    }

    // ---------------- pages ----------------

    fn overview(&mut self, ui: &mut egui::Ui, live: &sys::Live) {
        let cores = live.cores.max(1) as f64;
        ui.columns(4, |c| {
            card(&mut c[0], |ui| {
                ui.label(RichText::new("CPU").color(MUTED));
                ui.label(bold(format!("{:.0}%", live.cpu_pct), 30.0).color(TEXT));
                let h: Vec<f32> = live.cpu_hist.iter().cloned().collect();
                sparkline(ui, &h, 100.0, ACCENT, 46.0, &[]);
                ui.label(RichText::new(format!("{} cores \u{b7} last 2 minutes", live.cores)).size(12.0).color(MUTED));
            });
            card(&mut c[1], |ui| {
                ui.label(RichText::new("Memory").color(MUTED));
                ui.label(bold(format!("{:.1} GB", live.ram_used_mb / 1024.0), 30.0).color(TEXT));
                let h: Vec<f32> = live.ram_hist.iter().cloned().collect();
                sparkline(ui, &h, 100.0, BLUE, 46.0, &[]);
                ui.label(RichText::new(format!("of {:.0} GB in use", live.ram_total_mb / 1024.0)).size(12.0).color(MUTED));
            });
            let g = self.state.gpu.clone();
            card(&mut c[2], |ui| {
                ui.label(RichText::new("Graphics card").color(MUTED));
                if g.ok {
                    ui.label(bold(format!("{:.0}\u{b0}C", g.temp_c), 30.0).color(if g.temp_c >= 83.0 { BAD } else { TEXT }));
                    bar(ui, (g.mhz / g.max_mhz.max(1.0)) as f32, GOOD);
                    ui.label(RichText::new(format!("{:.0} of {:.0} MHz \u{b7} {:.0} W", g.mhz, g.max_mhz, g.power_w)).size(12.0).color(MUTED));
                    ui.label(RichText::new(if g.reasons.is_empty() { "not held back".to_string() } else { format!("held back by {}", g.reasons) }).size(12.0).color(if g.reasons.is_empty() { MUTED } else { WARN }));
                } else { ui.label(RichText::new("no recent reading").color(MUTED)); }
            });
            card(&mut c[3], |ui| {
                ui.label(RichText::new("Graphics memory").color(MUTED));
                if g.ok && g.total_mb > 0.0 {
                    let f = (g.used_mb / g.total_mb) as f32;
                    ui.label(bold(format!("{:.1} GB", g.used_mb / 1024.0), 30.0).color(if f > 0.92 { WARN } else { TEXT }));
                    bar(ui, f, if f > 0.92 { WARN } else { BLUE });
                    ui.label(RichText::new(format!("of {:.0} GB", g.total_mb / 1024.0)).size(12.0).color(MUTED));
                } else { ui.label(RichText::new("no recent reading").color(MUTED)); }
            });
        });
        ui.add_space(8.0);
        if !self.state.causes.is_empty() {
            card(ui, |ui| {
                section(ui, "What is slowing your PC", "");
                for c in &self.state.causes { ui.horizontal_wrapped(|ui| { ui.label(RichText::new("\u{2022}").color(ACCENT)); ui.label(c); }); }
            });
            ui.add_space(8.0);
        }
        let held = self.state.held.clone();
        let pending = self.state.pending.clone();
        let sugg = self.sugg.clone();
        ui.columns(2, |c| {
            card(&mut c[0], |ui| {
                section(ui, "Eased in the background", "Busy apps the governor slowed down so the app you are using stays fast.");
                if held.is_empty() { ui.label(RichText::new("Nothing is being held back right now.").color(MUTED)); }
                for h in held.iter().take(8) {
                    ui.horizontal(|ui| {
                        ui.label(bold(&h.name, 14.5).color(TEXT));
                        chip(ui, level_name(h.level), ACCENT);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Put back").on_hover_text("Restore it now and leave it alone from now on").clicked() {
                                self.run("snifrig-fix.exe", vec!["governor".into(), "undo".into(), h.pid.to_string()], &format!("{} is back to normal and will be left alone.", h.name));
                            }
                        });
                    });
                    ui.label(RichText::new(&h.reason).size(12.5).color(MUTED));
                    ui.separator();
                }
            });
            card(&mut c[1], |ui| {
                section(ui, "Waiting for your OK", "Fixes and suggestions that need you to decide.");
                if pending.is_empty() && sugg.is_empty() { ui.label(RichText::new("Nothing to decide.").color(MUTED)); }
                egui::ScrollArea::vertical().id_salt("pend").max_height(320.0).show(ui, |ui| {
                    for p in pending.iter().rev().take(20) { self.pending_card(ui, p, false); ui.add_space(4.0); }
                    for g in sugg.iter() { self.suggestion_card(ui, g, false); ui.add_space(4.0); }
                });
            });
        });
        ui.add_space(8.0);
        card(ui, |ui| {
            section(ui, "Busiest right now", "");
            egui::Grid::new("top").num_columns(4).striped(true).spacing([24.0, 6.0]).min_col_width(80.0).show(ui, |ui| {
                ui.label(RichText::new("App").color(MUTED)); ui.label(RichText::new("CPU").color(MUTED)); ui.label(RichText::new("Memory").color(MUTED)); ui.label(""); ui.end_row();
                for p in live.procs.iter().filter(|p| p.name != "System" && p.name != "Idle").take(8) {
                    ui.label(&p.name);
                    ui.label(format!("{:.1}%", p.core_pct / cores));
                    ui.label(format!("{:.0} MB", p.ws_mb));
                    if ui.small_button("Make a rule").clicked() {
                        let r = self.rules.rules.iter().find(|r| r.pattern == p.name.to_lowercase()).cloned();
                        let idx = self.rules.rules.iter().position(|r| r.pattern == p.name.to_lowercase());
                        self.edit = Some((idx, r.unwrap_or(Rule { pattern: p.name.to_lowercase(), ..Default::default() })));
                        self.page = Page::Rules;
                    }
                    ui.end_row();
                }
            });
        });
    }

    fn governor(&mut self, ui: &mut egui::Ui, live: &sys::Live) {
        ui.heading("Governor");
        ui.label(RichText::new("When your PC is pegged, the governor eases the busiest background apps in steps and puts them back when things calm down. It never slows the app you are using.").color(MUTED));
        ui.add_space(8.0);
        self.fixer_note(ui);
        card(ui, |ui| {
            section(ui, "Live", "CPU over the last 2 minutes, with the busy line (orange) and the calm line (green).");
            let h: Vec<f32> = live.cpu_hist.iter().cloned().collect();
            sparkline(ui, &h, 100.0, BLUE, 90.0, &[(self.tuning.busy_pct, ACCENT), (self.tuning.calm_pct, GOOD)]);
            ui.horizontal(|ui| {
                chip(ui, if self.state.busy { "busy: easing apps" } else { "not busy" }, if self.state.busy { ACCENT } else { GOOD });
                ui.label(RichText::new(format!("{} app{} eased right now", self.state.held.len(), if self.state.held.len() == 1 { "" } else { "s" })).color(MUTED));
            });
        });
        ui.add_space(8.0);
        let t = &mut self.tuning;
        ui.columns(2, |c| {
            card(&mut c[0], |ui| {
                section(ui, "When to step in", "");
                srow(ui, "Busy when total CPU is over", &mut t.busy_pct, 30.0..=100.0, "% total CPU");
                srow(ui, "For at least", &mut t.busy_secs, 1..=30, " s");
                ui.add_space(6.0);
                section(ui, "Who gets eased", "");
                srow(ui, "Only apps using more than", &mut t.min_core_pct, 5.0..=100.0, "% of one core");
                srow(ui, "Apps eased per second, at most", &mut t.max_per_tick, 1..=8, "");
                ui.horizontal(|ui| {
                    ui.label("Ease them down to");
                    for n in 1..=3u8 { ui.radio_value(&mut t.max_level, n, level_name(n)); }
                });
            });
            card(&mut c[1], |ui| {
                section(ui, "When to let go", "");
                srow(ui, "Calm when total CPU is under", &mut t.calm_pct, 10.0..=95.0, "% total CPU");
                srow(ui, "For at least", &mut t.calm_secs, 1..=120, " s");
                srow(ui, "Then put back one step every", &mut t.restore_every_secs, 1..=60, " s");
                ui.add_space(6.0);
                section(ui, "App rules", "");
                srow(ui, "Re-check rules every", &mut t.reapply_secs, 0..=300, " s");
                ui.label(RichText::new("Puts an app's priority and cores back if it changes them itself. 0 turns this off.").size(12.5).color(MUTED));
            });
        });
        if t.calm_pct > t.busy_pct { t.calm_pct = t.busy_pct; }
        self.save_bar(ui);
        ui.add_space(8.0);
        card(ui, |ui| {
            section(ui, "The three steps", "");
            for (n, d) in [(1u8, "Efficiency mode and low disk priority. Windows runs it on slower, cooler settings."), (2, "Below-normal priority. Your foreground app wins every contest for the CPU."), (3, "Idle priority and low memory priority. It only runs when nothing else wants to.")] {
                ui.horizontal(|ui| { chip(ui, &format!("{} \u{b7} {}", n, level_name(n)), ACCENT); ui.label(RichText::new(d).color(MUTED)); });
            }
        });
    }

    fn save_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let dirty = self.tuning != self.tuning_saved;
            if ui.add_enabled(dirty, egui::Button::new(RichText::new("Save changes").color(if dirty { Color32::BLACK } else { MUTED })).fill(if dirty { ACCENT } else { CARD2 })).clicked() { self.save_tuning(); }
            if ui.add_enabled(dirty, egui::Button::new("Undo changes")).clicked() { self.tuning = self.tuning_saved.clone(); }
            if ui.button("Back to defaults").clicked() { let plan = self.tuning.game_plan.clone(); self.tuning = Tuning { game_plan: plan, ..Tuning::default() }; }
            if dirty { ui.label(RichText::new("Unsaved changes").color(WARN)); }
        });
    }

    fn rules_page(&mut self, ui: &mut egui::Ui, live: &sys::Live) {
        ui.horizontal(|ui| {
            ui.heading("App rules");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.edit.is_none() && ui.add(egui::Button::new(RichText::new("+  New rule").color(Color32::BLACK)).fill(ACCENT)).clicked() { self.edit = Some((None, Rule::default())); }
            });
        });
        ui.label(RichText::new("Tell snifrig how to treat a specific app: always in the background, pinned to some cores, capped, never touched. Rules apply within 30 seconds and stick even if the app tries to undo them.").color(MUTED));
        ui.add_space(8.0);
        self.fixer_note(ui);
        if self.edit.is_some() { self.rule_editor(ui, live); ui.add_space(10.0); }
        if !self.state.suggestions.is_empty() && self.edit.is_none() {
            let sugg: Vec<(String, u32)> = self.state.suggestions.iter().filter(|(n, _)| !self.rules.rules.iter().any(|r| r.pattern == *n)).take(5).cloned().collect();
            if !sugg.is_empty() {
                card(ui, |ui| {
                    section(ui, "Suggestions", "The governor had to ease these often this week. Make it permanent, or tell it to leave them alone.");
                    for (n, c) in sugg {
                        ui.horizontal(|ui| {
                            ui.label(bold(&n, 14.5).color(TEXT));
                            ui.label(RichText::new(format!("eased {} times", c)).color(MUTED));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Leave it alone").clicked() { self.add_rule(Rule { pattern: n.clone(), governor: "exempt".into(), ..Default::default() }, &format!("snifrig will leave {} alone.", n)); }
                                if ui.button("Always background").clicked() { self.add_rule(Rule { pattern: n.clone(), priority: "below_normal".into(), efficiency: "on".into(), io: "low".into(), ..Default::default() }, &format!("{} will always run in the background.", n)); }
                            });
                        });
                    }
                });
                ui.add_space(8.0);
            }
        }
        if self.rules.rules.is_empty() {
            card(ui, |ui| { ui.label(RichText::new("No rules yet. Use \u{201c}New rule\u{201d}, or \u{201c}Make a rule\u{201d} next to a busy app on the Overview.").color(MUTED)); });
            return;
        }
        let mut edit = None;
        let mut delete = None;
        for (i, r) in self.rules.rules.iter().enumerate() {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(bold(&r.pattern, 15.5).color(TEXT));
                    let running = live.procs.iter().filter(|p| glob(&r.pattern, &p.name)).count();
                    if running > 0 { chip(ui, &format!("{} running", running), GOOD); }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.confirm_delete == Some(i) {
                            if ui.add(egui::Button::new(RichText::new("Delete").color(Color32::WHITE)).fill(BAD)).clicked() { delete = Some(i); }
                            if ui.button("Keep").clicked() { self.confirm_delete = None; }
                        } else {
                            if ui.button("Delete").clicked() { self.confirm_delete = Some(i); }
                            if ui.button("Edit").clicked() { edit = Some(i); }
                        }
                    });
                });
                ui.horizontal_wrapped(|ui| { for c in r.chips() { chip(ui, &c, BLUE); } });
            });
            ui.add_space(4.0);
        }
        if let Some(i) = edit { self.edit = Some((Some(i), self.rules.rules[i].clone())); }
        if let Some(i) = delete {
            let r = self.rules.rules.remove(i);
            self.confirm_delete = None;
            self.save_rules(&format!("Rule for {} deleted. Apps it changed keep their settings until they restart.", r.pattern));
        }
    }

    fn rule_editor(&mut self, ui: &mut egui::Ui, live: &sys::Live) {
        let Some((idx, mut r)) = self.edit.clone() else { return };
        let mut close = false;
        let mut save = false;
        let cores = live.cores.clamp(1, 64);
        card(ui, |ui| {
            section(ui, if idx.is_some() { "Edit rule" } else { "New rule" }, "");
            egui::Grid::new("rule").num_columns(2).spacing([18.0, 10.0]).show(ui, |ui| {
                ui.label("App");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut r.pattern).hint_text("name.exe, or a pattern like ffmpeg*.exe").desired_width(280.0));
                    let mut names: Vec<(String, f64)> = Vec::new();
                    for p in &live.procs { let n = p.name.to_lowercase(); if n != "system" && n != "idle" && !names.iter().any(|x| x.0 == n) { names.push((n, p.core_pct)); } }
                    egui::ComboBox::from_id_salt("pick").selected_text("Pick a running app").width(220.0).show_ui(ui, |ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.pick).hint_text("filter"));
                        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                            let f = self.pick.to_lowercase();
                            for (n, pct) in names.iter().filter(|(n, _)| f.is_empty() || n.contains(&f)).take(80) {
                                if ui.selectable_label(false, format!("{}   {:.0}%", n, pct / cores as f64)).clicked() { r.pattern = n.clone(); }
                            }
                        });
                    });
                });
                ui.end_row();
                let opts = |ui: &mut egui::Ui, id: &str, v: &mut String, choices: &[(&str, &str)]| {
                    let cur = choices.iter().find(|c| c.0 == v.as_str()).map(|c| c.1).unwrap_or("Don't change");
                    egui::ComboBox::from_id_salt(id).selected_text(cur).width(220.0).show_ui(ui, |ui| { for (k, l) in choices { ui.selectable_value(v, k.to_string(), *l); } });
                };
                ui.label("Priority");
                opts(ui, "prio", &mut r.priority, &[("", "Don't change"), ("idle", "Idle (lowest)"), ("below_normal", "Below normal"), ("normal", "Normal")]);
                ui.end_row();
                ui.label("Efficiency mode");
                opts(ui, "eff", &mut r.efficiency, &[("", "Don't change"), ("on", "On (slower, cooler)"), ("off", "Off")]);
                ui.end_row();
                ui.label("Disk priority");
                opts(ui, "io", &mut r.io, &[("", "Don't change"), ("very_low", "Very low"), ("low", "Low"), ("normal", "Normal")]);
                ui.end_row();
                ui.label("Memory priority");
                opts(ui, "mem", &mut r.memory, &[("", "Don't change"), ("very_low", "Very low"), ("low", "Low"), ("normal", "Normal")]);
                ui.end_row();
                ui.label("CPU cores");
                ui.vertical(|ui| {
                    let mut limit = r.affinity != 0;
                    if ui.checkbox(&mut limit, "Only run on these cores").changed() { r.affinity = if limit { (1u64 << cores.min(63)) - 1 } else { 0 }; }
                    if limit {
                        egui::Grid::new("cores").spacing([4.0, 4.0]).show(ui, |ui| {
                            for c in 0..cores {
                                let mut on = r.affinity >> c & 1 == 1;
                                if ui.toggle_value(&mut on, format!("{:>2}", c)).changed() { if on { r.affinity |= 1 << c } else { r.affinity &= !(1u64 << c) } }
                                if c % 8 == 7 { ui.end_row(); }
                            }
                        });
                        if r.affinity == 0 { ui.label(RichText::new("Pick at least one core.").color(WARN)); }
                    }
                });
                ui.end_row();
                ui.label("CPU limit");
                ui.horizontal(|ui| {
                    let mut on = r.cap > 0;
                    if ui.checkbox(&mut on, "Never more than").changed() { r.cap = if on { 50 } else { 0 }; }
                    if on { ui.add(egui::Slider::new(&mut r.cap, 1..=100).suffix("% of the whole CPU")); }
                });
                ui.end_row();
                ui.label("Copies");
                ui.horizontal(|ui| {
                    let mut on = r.max_instances > 0;
                    if ui.checkbox(&mut on, "Ask before more than").changed() { r.max_instances = if on { 1 } else { 0 }; }
                    if on { ui.add(egui::DragValue::new(&mut r.max_instances).range(1..=50)); ui.label("running at once"); }
                });
                ui.end_row();
                ui.label("Governor");
                let gov_now = match (r.governor.as_str(), r.max_level) { ("exempt", _) | (_, Some(0)) => 1, (_, Some(n)) => 2 + n as usize, ("on", _) => 2, _ => 0 };
                let gl = ["Default (hands off if this rule changes anything)", "Never touch it", "Manage it like any app", "Manage it, at most efficiency mode", "Manage it, at most below normal", "Manage it, at most idle"];
                let mut g = gov_now.min(5);
                egui::ComboBox::from_id_salt("gov").selected_text(gl[g]).width(330.0).show_ui(ui, |ui| { for (i, l) in gl.iter().enumerate() { ui.selectable_value(&mut g, i, *l); } });
                if g != gov_now { match g { 0 => { r.governor.clear(); r.max_level = None } 1 => { r.governor = "exempt".into(); r.max_level = None } 2 => { r.governor = "on".into(); r.max_level = None } n => { r.governor.clear(); r.max_level = Some((n - 2) as u8) } } }
                ui.end_row();
                ui.label("Game");
                opts(ui, "game", &mut r.game, &[("", "Detect automatically"), ("yes", "Always a game (performance mode)"), ("no", "Never a game")]);
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new(format!("Saved as:  {}", r.line())).monospace().size(12.0).color(MUTED));
            ui.horizontal(|ui| {
                let ok = !r.pattern.trim().is_empty() && !r.pattern.contains(char::is_whitespace) && !r.is_empty() && !(r.affinity == 0 && false);
                if ui.add_enabled(ok, egui::Button::new(RichText::new("Save rule").color(Color32::BLACK)).fill(ACCENT)).clicked() { save = true; }
                if ui.button("Cancel").clicked() { close = true; }
                if r.is_empty() && !r.pattern.is_empty() { ui.label(RichText::new("Choose at least one setting.").color(MUTED)); }
            });
        });
        r.pattern = r.pattern.trim().to_lowercase();
        if save {
            match idx { Some(i) if i < self.rules.rules.len() => self.rules.rules[i] = r.clone(), _ => self.rules.rules.push(r.clone()) }
            self.edit = None;
            self.save_rules(&format!("Rule for {} saved. snifrig fix applies it within 30 seconds.", r.pattern));
        } else if close { self.edit = None; } else { self.edit = Some((idx, r)); }
    }

    fn performance(&mut self, ui: &mut egui::Ui) {
        ui.heading("Performance mode");
        ui.label(RichText::new("When a game is in front, snifrig switches to your fastest power plan, eases background apps sooner, and never slows the game, even when you alt-tab. Your plan comes back when the game closes.").color(MUTED));
        ui.add_space(8.0);
        self.fixer_note(ui);
        if self.plans.is_empty() { self.plans = data::power_plans(); }
        card(ui, |ui| {
            ui.horizontal(|ui| {
                match &self.state.game { Some(g) => { chip(ui, "ON", GOOD); ui.label(bold(format!("Running for {}", g), 16.0).color(TEXT)); } None => { chip(ui, "waiting", MUTED); ui.label(RichText::new("No game in front right now.").color(MUTED)); } }
            });
            if let Some((_, n, _)) = self.plans.iter().find(|p| p.2) { ui.label(RichText::new(format!("Current power plan: {}", n)).color(MUTED)); }
        });
        ui.add_space(8.0);
        let plans = self.plans.clone();
        let t = &mut self.tuning;
        card(ui, |ui| {
            ui.checkbox(&mut t.game_mode, bold("Turn on performance mode when a game is in front", 15.0));
            ui.add_enabled_ui(t.game_mode, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Power plan while gaming");
                    let label = match t.game_plan.as_str() { "auto" | "" => "Fastest available".to_string(), "none" => "Don't change the plan".to_string(), g => plans.iter().find(|p| p.0 == g || p.1.to_lowercase() == g).map(|p| p.1.clone()).unwrap_or(g.to_string()) };
                    egui::ComboBox::from_id_salt("plan").selected_text(label).width(260.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut t.game_plan, "auto".to_string(), "Fastest available");
                        ui.selectable_value(&mut t.game_plan, "none".to_string(), "Don't change the plan");
                        for (g, n, _) in &plans { ui.selectable_value(&mut t.game_plan, g.clone(), n); }
                    });
                });
                srow(ui, "While gaming, ease background apps from", &mut t.game_busy_pct, 20.0..=100.0, "% total CPU");
            });
        });
        self.save_bar(ui);
        ui.add_space(8.0);
        card(ui, |ui| {
            section(ui, "Games snifrig knows", "Found by the local brain, or marked by you. Change any of them.");
            let mut names: Vec<(String, String)> = self.state.brain.iter().filter(|b| b.kind == "game" && b.confidence >= 0.35).map(|b| (b.name.clone(), "found by the brain".to_string())).collect();
            for r in &self.rules.rules { if !r.game.is_empty() && !names.iter().any(|n| n.0 == r.pattern) { names.push((r.pattern.clone(), "your rule".into())); } }
            if names.is_empty() { ui.label(RichText::new("None yet. Play something, or mark an app as a game in App rules.").color(MUTED)); }
            let mut change: Option<(String, String)> = None;
            egui::Grid::new("games").num_columns(3).spacing([24.0, 8.0]).show(ui, |ui| {
                for (n, src) in &names {
                    ui.label(bold(n, 14.5).color(TEXT));
                    ui.label(RichText::new(src).color(MUTED));
                    let rule = self.rules.rules.iter().find(|r| r.pattern == *n);
                    let mut v = rule.map(|r| r.game.clone()).unwrap_or_default();
                    let slows = rule.map_or(false, |r| !r.priority.is_empty() && r.priority != "normal" || r.efficiency == "on" || r.cap > 0 || r.affinity != 0);
                    let cur = match v.as_str() { "yes" => "Always a game", "no" => "Never a game", _ if slows => "Off (your rule slows it)", _ => "Detect automatically" };
                    let before = v.clone();
                    egui::ComboBox::from_id_salt(format!("g-{}", n)).selected_text(cur).width(200.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut v, String::new(), "Detect automatically");
                        ui.selectable_value(&mut v, "yes".to_string(), "Always a game");
                        ui.selectable_value(&mut v, "no".to_string(), "Never a game");
                    });
                    if v != before { change = Some((n.clone(), v)); }
                    ui.end_row();
                }
            });
            if let Some((n, v)) = change {
                let mut r = self.rules.rules.iter().find(|r| r.pattern == n).cloned().unwrap_or(Rule { pattern: n.clone(), ..Default::default() });
                r.game = v;
                if r.is_empty() { self.rules.rules.retain(|x| x.pattern != n); self.save_rules(&format!("{} is detected automatically again.", n)); }
                else { self.add_rule(r, &format!("Saved for {}.", n)); }
            }
        });
    }

    fn activity(&mut self, ui: &mut egui::Ui) {
        ui.heading("Activity");
        ui.label(RichText::new("Everything snifrig fix did, would have done, or decided not to do, newest first.").color(MUTED));
        ui.add_space(8.0);
        if self.state.mem_hist.len() > 2 {
            card(ui, |ui| {
                section(ui, "Memory, last 24 hours", "Committed memory (orange) and free RAM (blue), in GB.");
                let commit: Vec<f32> = self.state.mem_hist.iter().map(|x| x.1).collect();
                let avail: Vec<f32> = self.state.mem_hist.iter().map(|x| x.2).collect();
                let max = commit.iter().chain(avail.iter()).cloned().fold(1.0f32, f32::max) * 1.1;
                let top = ui.cursor().top();
                sparkline(ui, &commit, max, ACCENT, 110.0, &[]);
                let r = egui::Rect::from_min_size(egui::pos2(ui.min_rect().left(), top), Vec2::new(ui.available_width(), 110.0));
                let child = ui.new_child(egui::UiBuilder::new().max_rect(r));
                let pts: Vec<f32> = avail;
                let n = pts.len().max(2) as f32 - 1.0;
                let line: Vec<egui::Pos2> = pts.iter().enumerate().map(|(i, v)| egui::pos2(r.left() + i as f32 / n * r.width(), r.bottom() - (v / max).clamp(0.0, 1.0) * (r.height() - 4.0) - 2.0)).collect();
                child.painter().add(egui::Shape::line(line, Stroke::new(2.0, BLUE)));
                ui.horizontal(|ui| {
                    let first = self.state.mem_hist.first().map(|x| x.0).unwrap_or(0.0);
                    ui.label(RichText::new(local_time(first, true)).size(12.0).color(MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(RichText::new("now").size(12.0).color(MUTED)); });
                });
            });
            ui.add_space(8.0);
        }
        ui.horizontal(|ui| {
            for (f, l) in [(Filter::All, "All"), (Filter::Governor, "Governor"), (Filter::Rules, "Rules"), (Filter::Fixes, "Fixes"), (Filter::Performance, "Performance"), (Filter::Learning, "Learning")] {
                ui.selectable_value(&mut self.filter, f, l);
            }
            ui.add_space(16.0);
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("search apps or notes").desired_width(220.0));
        });
        ui.add_space(4.0);
        let q = self.search.to_lowercase();
        let filter = self.filter;
        let evs: Vec<&data::Event> = self.state.events.iter().filter(|e| {
            let cat = if e.key.starts_with("gov:") { Filter::Governor } else if e.key.starts_with("rule:") { Filter::Rules } else if e.key.starts_with("perf:") { Filter::Performance } else if e.key.starts_with("learn:") { Filter::Learning } else { Filter::Fixes };
            (filter == Filter::All || filter == cat) && (q.is_empty() || e.name.to_lowercase().contains(&q) || e.note.to_lowercase().contains(&q))
        }).collect();
        card(ui, |ui| {
            if evs.is_empty() { ui.label(RichText::new("Nothing here yet.").color(MUTED)); return; }
            egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                for e in evs.iter().take(400) {
                    ui.horizontal(|ui| {
                        let c = match e.status.as_str() { "done" => GOOD, "failed" => BAD, "refused" => WARN, "queued" => ACCENT, _ => MUTED };
                        ui.label(RichText::new(local_time(e.unix, true)).size(12.5).color(MUTED).monospace());
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, c);
                        if !e.name.is_empty() { ui.label(bold(&e.name, 13.5).color(TEXT)); }
                        let note = e.note.trim_start_matches("governor: ").trim_start_matches("rule: ");
                        ui.label(RichText::new(note).size(13.0).color(if e.status == "would" { MUTED } else { TEXT }));
                    });
                }
            });
        });
    }

    fn report(&mut self, ui: &mut egui::Ui) {
        ui.heading("Report");
        ui.label(RichText::new("What slowed this PC down, and what snifrig fix did about it. Free, made from the monitor's own records.").color(MUTED));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for d in [1u32, 7, 30] { if ui.selectable_value(&mut self.report_days, d, format!("{} day{}", d, if d == 1 { "" } else { "s" })).clicked() { *self.report.lock().unwrap() = None; } }
        });
        let have = self.report.lock().unwrap().clone();
        match have {
            None => {
                *self.report.lock().unwrap() = Some("Working...".into());
                let (path, out, dir, days) = (data::tool("snifrig.exe", &self.dir), self.report.clone(), self.dir.clone(), self.report_days);
                std::thread::spawn(move || {
                    let r = data::cmd(&path.to_string_lossy(), &["report", "--days", &days.to_string(), "--dir", &dir.to_string_lossy()]).unwrap_or_else(|e| e);
                    *out.lock().unwrap() = Some(r);
                });
            }
            Some(text) => {
                let mut blocks: Vec<(String, Vec<String>)> = Vec::new();
                for l in text.lines() {
                    if let Some(h) = l.strip_prefix("- ") { blocks.push((h.to_string(), Vec::new())); }
                    else if l.starts_with("    ") { if let Some(b) = blocks.last_mut() { b.1.push(l.trim().to_string()); } }
                    else if l.starts_with("snifrig fix on this PC") || l.starts_with("snifrig fix (dry run)") || l.starts_with("Nothing slowed") { blocks.push((l.to_string(), vec![])); }
                }
                if blocks.is_empty() { card(ui, |ui| { ui.label(RichText::new(text.trim()).color(MUTED)); }); }
                egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                    for (h, lines) in &blocks {
                        card(ui, |ui| {
                            ui.label(bold(h, 15.0).color(TEXT));
                            for l in lines {
                                if let Some(f) = l.strip_prefix("snifrig fix: ") { ui.horizontal_wrapped(|ui| { chip(ui, "snifrig fix", ACCENT); ui.label(RichText::new(f).color(MUTED)); }); }
                                else { ui.label(RichText::new(l).color(MUTED)); }
                            }
                        });
                        ui.add_space(4.0);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Copy report").clicked() { ui.ctx().copy_text(text.clone()); self.say("Report copied.", true); }
                        if ui.button("Refresh").clicked() { *self.report.lock().unwrap() = None; }
                    });
                });
            }
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add_space(8.0);
        card(ui, |ui| {
            section(ui, "What snifrig fix may do", "");
            if !self.state.has_key { ui.label(RichText::new("snifrig fix needs a license. The monitor, the report and this window are free.").color(WARN)); }
            ui.add_enabled_ui(self.state.has_key, |ui| {
                let mut m = self.state.mode.clone();
                for (k, d) in [("off", "Do nothing."), ("dry-run", "Watch only: log what it would do, change nothing."), ("ask", "Ease busy apps and apply your rules by itself; ask before ending or restarting anything."), ("auto", "Also trim and lower leaking apps by itself; end or restart only apps you allowed.")] {
                    ui.horizontal(|ui| { ui.radio_value(&mut m, k.to_string(), bold(mode_label(k), 14.5)); ui.label(RichText::new(d).color(MUTED)); });
                }
                if m != self.state.mode { self.state.mode = m.clone(); self.run("snifrig-fix.exe", vec!["mode".into(), m.clone()], &format!("Fixer set to {}.", mode_label(&m))); }
            });
        });
        ui.add_space(8.0);
        ui.columns(2, |c| {
            card(&mut c[0], |ui| {
                section(ui, "License", "");
                let l = self.license.lock().unwrap().clone();
                ui.label(RichText::new(if l.is_empty() { "checking..." } else { l.as_str() }).color(MUTED));
            });
            card(&mut c[1], |ui| {
                section(ui, "Local brain", "An optional AI on this PC that tells snifrig what each app is (a game, a batch job, an AI model...). Nothing leaves your machine.");
                if self.brain_up.is_none() { self.brain_up = Some(std::net::TcpStream::connect_timeout(&"127.0.0.1:8791".parse().unwrap(), Duration::from_millis(250)).is_ok()); }
                ui.horizontal(|ui| {
                    if self.brain_up == Some(true) { chip(ui, "running", GOOD); } else { chip(ui, "not running", MUTED); }
                    ui.label(RichText::new(format!("knows {} apps", self.state.brain.len())).color(MUTED));
                });
            });
        });
        ui.add_space(8.0);
        card(ui, |ui| {
            section(ui, "Data folder", "Everything snifrig knows stays here, on this PC.");
            ui.horizontal(|ui| {
                ui.label(RichText::new(self.dir.to_string_lossy()).monospace().color(MUTED));
                if ui.button("Open folder").clicked() { let _ = std::process::Command::new("explorer").arg(&self.dir).spawn(); }
            });
        });
        ui.add_space(8.0);
        card(ui, |ui| {
            ui.label(RichText::new(format!("snifrig {} \u{b7} snifrig snif is free, snifrig fix is paid \u{b7} UFL 3.7", env!("CARGO_PKG_VERSION"))).color(MUTED));
        });
    }
}

fn glob(p: &str, name: &str) -> bool {
    let n = name.to_lowercase();
    if !p.contains('*') && !p.contains('?') { return p == n; }
    fn m(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => m(&p[1..], n) || (!n.is_empty() && m(p, &n[1..])),
            (Some(b'?'), Some(_)) => m(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => m(&p[1..], &n[1..]),
            _ => false,
        }
    }
    m(p.as_bytes(), n.as_bytes())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.loaded.elapsed() > Duration::from_millis(1500) { self.reload(); }
        if self.ask { self.ask_view(ctx); return; }
        let live = self.live.lock().map(|g| g.clone()).unwrap_or_default();
        egui::SidePanel::left("nav").exact_width(220.0).resizable(false).frame(egui::Frame::new().fill(SIDE).inner_margin(Margin::symmetric(12, 0))).show(ctx, |ui| self.nav(ui));
        egui::TopBottomPanel::top("top").frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(20, 6)).stroke(Stroke::new(1.0, BORDER))).show(ctx, |ui| self.top_bar(ui));
        let toast = self.toast.lock().unwrap().clone();
        if let Some((msg, ok, at)) = toast {
            if at.elapsed() < Duration::from_secs(6) {
                egui::TopBottomPanel::bottom("toast").frame(egui::Frame::new().fill(if ok { Color32::from_rgb(24, 52, 40) } else { Color32::from_rgb(64, 28, 26) }).inner_margin(Margin::symmetric(20, 10))).show(ctx, |ui| {
                    ui.label(RichText::new(msg).color(TEXT));
                });
            }
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(24, 18))).show(ctx, |ui| {
            egui::ScrollArea::vertical().id_salt(format!("page{}", self.page as u8)).auto_shrink([false, false]).show(ui, |ui| {
                match self.page {
                    Page::Overview => self.overview(ui, &live),
                    Page::Governor => self.governor(ui, &live),
                    Page::Rules => self.rules_page(ui, &live),
                    Page::Performance => self.performance(ui),
                    Page::Activity => self.activity(ui),
                    Page::Report => self.report(ui),
                    Page::Settings => self.settings(ui),
                }
            });
        });
        ctx.request_repaint_after(Duration::from_millis(1000));
    }
}

#[allow(dead_code)]
fn _unused(_: VecDeque<f32>) {}

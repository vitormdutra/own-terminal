use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui::{self, FontId, Key, Pos2, Rect, Vec2};

mod config;
mod layout;
mod pane;

use config::*;
use layout::*;
use pane::*;

const TAB_H: f32 = 26.0;

const NUMS: [Key; 9] = [
    Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9,
];

const SEARCH_H: f32 = 28.0;

/// Where the item at `active` ends up after the item at `from` is moved to `to`.
fn moved_index(active: usize, from: usize, to: usize) -> usize {
    if active == from { to }
    else if from < active && to >= active { active - 1 }
    else if from > active && to <= active { active + 1 }
    else { active }
}

struct App {
    tabs: Vec<Tab>,
    active: usize,
    title: String,
    font_size: f32,
    search: Option<String>,
    search_new: bool,
    search_focus: bool,
    rects: Vec<Rect>, // last frame's pane rects of the active tab, for directional focus
}

#[derive(Default)]
struct Keys {
    new: bool,
    close: bool,
    goto: Option<usize>,
    split: Option<bool>, // Some(stacked)
    step: isize,
    zoom: i32,
    reset: bool,
    find: bool,
    maximize: bool,
    esc: bool,
    reload: bool,
    dir: Option<(i32, i32)>,
}

impl App {
    /// Directory of the focused pane, so new tabs and splits start where you are.
    fn focused_cwd(&self) -> Option<std::path::PathBuf> {
        let t = self.tabs.get(self.active)?;
        let mut ps = Vec::new();
        t.root.panes(&mut ps);
        ps.get(t.focus)?.cwd()
    }

    fn new_tab(&mut self, ctx: &egui::Context) {
        if let Ok(p) = Pane::spawn(ctx, self.focused_cwd()) {
            self.tabs.push(Tab::new(p));
            self.active = self.tabs.len() - 1;
        }
    }

    fn close_search(&mut self) {
        self.search = None;
        self.search_focus = false;
        for t in &mut self.tabs {
            let mut ps = Vec::new();
            t.root.panes_mut(&mut ps);
            for p in ps { p.matches.clear(); }
        }
    }
}

/// With OWN_TERMINAL_PROFILE=1, prints frames/s and the average and worst UI time once per second to stderr.
struct FrameTimer(Instant);

impl Drop for FrameTimer {
    fn drop(&mut self) {
        static ON: OnceLock<bool> = OnceLock::new();
        static STATS: Mutex<Option<(Instant, u32, Duration, Duration)>> = Mutex::new(None);
        if !*ON.get_or_init(|| std::env::var_os("OWN_TERMINAL_PROFILE").is_some()) { return; }
        let took = self.0.elapsed();
        let mut g = STATS.lock().unwrap();
        let (since, n, total, worst) = g.get_or_insert((Instant::now(), 0, Duration::ZERO, Duration::ZERO));
        *n += 1;
        *total += took;
        *worst = (*worst).max(took);
        if since.elapsed() >= Duration::from_secs(1) {
            eprintln!("frames/s {n}  avg {:.2} ms  worst {:.2} ms", total.as_secs_f64() * 1000.0 / *n as f64, worst.as_secs_f64() * 1000.0);
            *g = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let _timer = FrameTimer(Instant::now());
        self.tabs.retain_mut(|t| {
            let gone = t.root.prune();
            t.focus = t.focus.min(t.root.count().saturating_sub(1));
            !gone
        });
        if self.tabs.is_empty() {
            ui.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.active = self.active.min(self.tabs.len() - 1);

        // Long-running commands: notify when one finishes while the window is in the background.
        let win_focused = ui.input(|i| i.viewport().focused.unwrap_or(true));
        for t in &mut self.tabs {
            let mut ps = Vec::new();
            t.root.panes_mut(&mut ps);
            for p in ps {
                if let Some(secs) = p.poll(cfg().notify_after) {
                    if !win_focused {
                        let title = p.title.lock().unwrap().clone();
                        notify(&format!("Terminou após {secs}s {title}").trim_end().to_string());
                        ui.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
                    }
                }
            }
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1));

        // Cmd+T tab, Cmd+W close pane, Cmd+D split (+Shift: stacked), Cmd+[ / ] focus, Cmd+1..9 tab,
        // Cmd+= / - / 0 zoom, Cmd+F search. ("Cmd" is Ctrl+Shift off macOS.)
        let mut k = Keys::default();
        ui.input(|i| {
            k.esc = i.key_pressed(Key::Escape);
            if !is_app_shortcut(&i.modifiers) { return; }
            k.new = i.key_pressed(Key::T);
            k.close = i.key_pressed(Key::W);
            k.goto = NUMS.iter().position(|n| i.key_pressed(*n));
            // Stacked split: Cmd+Shift+D on macOS, Ctrl+Shift+Alt+D elsewhere (Shift is always down there).
            k.split = i.key_pressed(Key::D).then_some(if cfg!(target_os = "macos") { i.modifiers.shift } else { i.modifiers.alt });
            k.step = i.key_pressed(Key::CloseBracket) as isize - i.key_pressed(Key::OpenBracket) as isize;
            k.zoom = (i.key_pressed(Key::Equals) || i.key_pressed(Key::Plus)) as i32 - i.key_pressed(Key::Minus) as i32;
            k.reset = i.key_pressed(Key::Num0);
            k.find = i.key_pressed(Key::F);
            k.maximize = i.key_pressed(Key::Enter) && i.modifiers.shift;
            k.reload = i.key_pressed(Key::Comma);
            if i.modifiers.alt {
                k.dir = [(Key::ArrowLeft, (-1, 0)), (Key::ArrowRight, (1, 0)), (Key::ArrowUp, (0, -1)), (Key::ArrowDown, (0, 1))]
                    .iter()
                    .find(|(key, _)| i.key_pressed(*key))
                    .map(|(_, d)| *d);
            }
        });
        if k.new { self.new_tab(ui.ctx()); }
        if k.close {
            let t = &mut self.tabs[self.active];
            let mut ps = Vec::new();
            t.root.panes_mut(&mut ps);
            ps[t.focus.min(ps.len() - 1)].dead.store(true, Ordering::Relaxed);
            if t.root.prune() { self.tabs.remove(self.active); }
            if self.tabs.is_empty() { return; }
            self.active = self.active.min(self.tabs.len() - 1);
        }
        if let Some(n) = k.goto.filter(|n| *n < self.tabs.len()) { self.active = n; }
        if let Some(stacked) = k.split {
            if let Ok(p) = Pane::spawn(ui.ctx(), self.focused_cwd()) {
                let t = &mut self.tabs[self.active];
                t.root.split(&mut t.focus.clone(), &mut Some(p), stacked);
                t.focus += 1;
                t.zoom = false;
            }
        }
        if k.maximize { self.tabs[self.active].zoom ^= true; }
        if k.reload {
            reload();
            load_font(ui.ctx());
            self.font_size = cfg().font_size;
        }
        if k.zoom != 0 { self.font_size = (self.font_size + k.zoom as f32).clamp(8.0, 40.0); }
        if k.reset { self.font_size = cfg().font_size; }
        if k.find {
            self.search.get_or_insert_with(String::new);
            self.search_new = true;
        }
        if k.esc && self.search.is_some() { self.close_search(); }

        let t = &mut self.tabs[self.active];
        let n = t.root.count();
        let multi = n > 1;
        t.focus = t.focus.min(n - 1);
        if let Some((dx, dy)) = k.dir {
            if self.rects.len() == n {
                if let Some(j) = nav(&self.rects, t.focus, dx, dy) { t.focus = j; }
            }
        }
        t.focus = (t.focus as isize + k.step).rem_euclid(n as isize) as usize;

        let mut ps = Vec::new();
        t.root.panes(&mut ps);
        let title = ps[t.focus].title.lock().unwrap().clone();
        let title = if title.is_empty() { "own-terminal".to_string() } else { title };
        if title != self.title {
            ui.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        let font = FontId::monospace(self.font_size);
        let (cw, ch) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let full = ui.available_rect_before_wrap();
        let (bar, rest) = (
            Rect::from_min_size(full.min, Vec2::new(full.width(), TAB_H)),
            Rect::from_min_max(Pos2::new(full.left(), full.top() + TAB_H), full.max),
        );
        let sh = if self.search.is_some() { SEARCH_H } else { 0.0 };
        let (body, sbar) = (
            Rect::from_min_max(rest.min, Pos2::new(rest.right(), rest.bottom() - sh)),
            Rect::from_min_max(Pos2::new(rest.left(), rest.bottom() - sh), rest.max),
        );

        // Tab bar: one rect per tab plus a "+" button.
        let p = ui.painter_at(bar);
        p.rect_filled(bar, 0.0, cfg().bar);
        let w = (bar.width() / (self.tabs.len() as f32 + 1.0)).min(180.0);
        let mut spawn = false;
        let (mut close_tab, mut reorder) = (None, None);
        for (n, tab) in self.tabs.iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(bar.left() + n as f32 * w, bar.top()), Vec2::new(w, TAB_H));
            let resp = ui.interact(r, ui.scope_id().with(("tab", tab.id)), egui::Sense::CLICK | egui::Sense::DRAG);
            let x = Rect::from_center_size(Pos2::new(r.right() - 14.0, r.center().y), Vec2::splat(18.0));
            let xr = ui.interact(x, ui.scope_id().with(("close", tab.id)), egui::Sense::CLICK);
            if xr.clicked() {
                close_tab = Some(n);
            } else if resp.clicked() {
                self.active = n;
            }
            if let (true, Some(pos)) = (resp.dragged(), resp.interact_pointer_pos()) {
                let to = (((pos.x - bar.left()) / w) as usize).min(self.tabs.len() - 1);
                if to != n { reorder = Some((n, to)); }
            }
            if n == self.active {
                p.rect_filled(r, 0.0, cfg().bg);
                p.rect_filled(Rect::from_min_size(r.min, Vec2::new(r.width(), 2.0)), 0.0, cfg().accent);
            }
            let mut ps = Vec::new();
            tab.root.panes(&mut ps);
            let title = ps[tab.focus.min(ps.len() - 1)].title.lock().unwrap().clone();
            let title = if title.is_empty() { format!("{}", n + 1) } else { title };
            let title = if tab.zoom && ps.len() > 1 { format!("[zoom] {title}") } else { title };
            let on = n == self.active;
            p.with_clip_rect(Rect::from_min_max(r.min + Vec2::new(6.0, 0.0), r.max - Vec2::new(26.0, 0.0))).text(
                Pos2::new(r.left() + 8.0, r.center().y),
                egui::Align2::LEFT_CENTER,
                title,
                FontId::proportional(12.0),
                if on { cfg().fg } else { cfg().fg.gamma_multiply(0.55) },
            );
            if on || resp.hovered() || xr.hovered() {
                let c = if xr.hovered() { cfg().fg } else { cfg().fg.gamma_multiply(0.5) };
                p.text(x.center(), egui::Align2::CENTER_CENTER, "×", FontId::proportional(14.0), c);
            }
        }
        if let Some(n) = close_tab {
            self.tabs.remove(n); // dropping the tab shuts its shells down
            if n < self.active { self.active -= 1; }
            if self.tabs.is_empty() { return; }
            self.active = self.active.min(self.tabs.len() - 1);
        }
        if let Some((from, to)) = reorder {
            let tab = self.tabs.remove(from);
            self.tabs.insert(to, tab);
            self.active = moved_index(self.active, from, to);
        }
        let plus = Rect::from_min_size(Pos2::new(bar.left() + self.tabs.len() as f32 * w, bar.top()), Vec2::splat(TAB_H));
        if ui.interact(plus, ui.scope_id().with("plus"), egui::Sense::CLICK).clicked() { spawn = true; }
        p.text(plus.center(), egui::Align2::CENTER_CENTER, "+", FontId::proportional(14.0), cfg().fg);
        if spawn { self.new_tab(ui.ctx()); }
        if self.active >= self.tabs.len() { return; }

        // Panes (and their draggable dividers) fill the body.
        ui.painter_at(body).rect_filled(body, 0.0, cfg().bar);
        let zoomed = multi && self.tabs[self.active].zoom;
        let view = View { font: &font, cw, ch, hl: self.search.is_some(), block: self.search_focus, multi: multi && !zoomed };
        let t = &mut self.tabs[self.active];
        if zoomed {
            let mut ps = Vec::new();
            t.root.panes_mut(&mut ps);
            let f = t.focus.min(ps.len() - 1);
            ps[f].show(ui, body, &view, true, 0);
            self.rects = vec![]; // no directional focus while maximized
        } else {
            let mut walk = Walk { idx: 0, focus: t.focus, new_focus: None, div: 0, rects: vec![] };
            t.root.show(ui, body, &view, &mut walk);
            if let Some(f) = walk.new_focus { t.focus = f; }
            self.rects = walk.rects;
        }

        // Search bar: Enter = previous (older) match, Shift+Enter = next.
        if let Some(mut q) = self.search.take() {
            ui.painter_at(sbar).rect_filled(sbar, 0.0, cfg().bar);
            let mut ps = Vec::new();
            t.root.panes_mut(&mut ps);
            let pane = &mut ps[t.focus.min(n - 1)];
            let te = Rect::from_min_max(Pos2::new(sbar.left() + 8.0, sbar.top() + 2.0), Pos2::new(sbar.right() - 80.0, sbar.bottom() - 2.0));
            let r = ui.put(te, egui::TextEdit::singleline(&mut q).hint_text("Buscar (Enter: anterior, Shift+Enter: próximo, Esc: fechar)"));
            if self.search_new {
                r.request_focus();
                self.search_new = false;
            }
            if r.changed() {
                pane.search(&q);
                pane.jump(0);
            }
            let (enter, shift) = ui.input(|i| (i.key_pressed(Key::Enter), i.modifiers.shift));
            if r.lost_focus() && enter {
                pane.jump(if shift { 1 } else { -1 });
                r.request_focus();
            }
            self.search_focus = r.has_focus();
            let count = if pane.matches.is_empty() { "0/0".to_string() } else { format!("{}/{}", pane.match_idx + 1, pane.matches.len()) };
            ui.painter_at(sbar).text(
                Pos2::new(sbar.right() - 8.0, sbar.center().y),
                egui::Align2::RIGHT_CENTER,
                count,
                FontId::proportional(12.0),
                cfg().fg,
            );
            self.search = Some(q);
        }
    }
}

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("own-terminal", opts, Box::new(|cc| {
        load_font(&cc.egui_ctx);
        Ok(Box::new(App {
            tabs: vec![Tab::new(Pane::spawn(&cc.egui_ctx, None)?)],
            active: 0,
            title: String::new(),
            font_size: cfg().font_size,
            search: None,
            search_new: false,
            search_focus: false,
            rects: vec![],
        }))
    }))
}

#[cfg(test)]
mod tab_tests {
    use super::moved_index;

    #[test]
    fn active_tab_follows_a_move() {
        // tabs [a b c d], moving b (1) to the end (3) -> [a c d b]
        assert_eq!(moved_index(1, 1, 3), 3); // the moved tab was active
        assert_eq!(moved_index(2, 1, 3), 1); // c shifts left
        assert_eq!(moved_index(0, 1, 3), 0); // a untouched
        // moving d (3) to the front (0) -> [d a b c]
        assert_eq!(moved_index(1, 3, 0), 2); // b shifts right
    }
}

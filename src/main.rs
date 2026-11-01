use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{cell::Flags, Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, CursorShape, CursorStyle, NamedColor};
use eframe::egui::{self, Color32, FontId, Key, Pos2, Rect, Vec2};

const TAB_H: f32 = 26.0;

// User settings from ~/.config/own-terminal/config: `key = value` lines, `#` comments.
// Keys: font_size, font_path (.ttf/.otf), fg, bg (#rrggbb), cursor (block|bar|underline), cursor_blink (true|false).
struct Settings {
    font_size: f32,
    font_path: Option<String>,
    fg: Color32,
    bg: Color32,
    bar: Color32,
    cursor: CursorStyle,
}

fn cfg() -> &'static Settings {
    static CFG: OnceLock<Settings> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut s = Settings {
            font_size: 14.0,
            font_path: None,
            fg: Color32::from_rgb(0xd8, 0xd8, 0xd8),
            bg: Color32::from_rgb(0x18, 0x18, 0x18),
            bar: Color32::BLACK,
            cursor: CursorStyle { shape: CursorShape::Block, blinking: false },
        };
        let path = std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config/own-terminal/config"));
        let text = path.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let hex = |v: &str| {
            let n = u32::from_str_radix(v.trim_start_matches('#'), 16).ok()?;
            Some(Color32::from_rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
        };
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "font_size" => s.font_size = v.parse().unwrap_or(s.font_size),
                "font_path" => s.font_path = Some(v.to_string()),
                "fg" => s.fg = hex(v).unwrap_or(s.fg),
                "bg" => s.bg = hex(v).unwrap_or(s.bg),
                "cursor" => match v {
                    "block" => s.cursor.shape = CursorShape::Block,
                    "bar" => s.cursor.shape = CursorShape::Beam,
                    "underline" => s.cursor.shape = CursorShape::Underline,
                    _ => {}
                },
                "cursor_blink" => s.cursor.blinking = v == "true",
                _ => {}
            }
        }
        s.bar = s.bg.gamma_multiply(0.6);
        s
    })
}

fn load_font(ctx: &egui::Context) {
    let Some(bytes) = cfg().font_path.as_ref().and_then(|p| std::fs::read(p).ok()) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("custom".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
    fonts.families.entry(egui::FontFamily::Monospace).or_default().insert(0, "custom".into());
    ctx.set_fonts(fonts);
}

const ANSI: [u32; 16] = [
    0x181818, 0xab4642, 0xa1b56c, 0xf7ca88, 0x7cafc2, 0xba8baf, 0x86c1b9, 0xd8d8d8, //
    0x585858, 0xdc5c5c, 0xb5d37f, 0xffd98d, 0x8ec6dc, 0xd29ec8, 0x9bd6cd, 0xf8f8f8,
];
const NUMS: [Key; 9] = [
    Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9,
];

struct Size {
    cols: usize,
    rows: usize,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize { self.rows }
    fn screen_lines(&self) -> usize { self.rows }
    fn columns(&self) -> usize { self.cols }
}

// Bridges one tab's terminal events to egui (repaint/title/exit) and back to its pty (query replies).
#[derive(Clone)]
struct Proxy {
    ctx: egui::Context,
    tx: Arc<OnceLock<EventLoopSender>>,
    title: Arc<Mutex<String>>,
    dead: Arc<AtomicBool>,
}
impl EventListener for Proxy {
    fn send_event(&self, e: Event) {
        match e {
            Event::Wakeup => self.ctx.request_repaint(),
            Event::Title(t) => *self.title.lock().unwrap() = t,
            Event::ResetTitle => self.title.lock().unwrap().clear(),
            Event::PtyWrite(s) => {
                if let Some(tx) = self.tx.get() {
                    let _ = tx.send(Msg::Input(s.into_bytes().into()));
                }
            }
            Event::Exit | Event::ChildExit(_) => {
                self.dead.store(true, Ordering::Relaxed);
                self.ctx.request_repaint();
            }
            _ => {}
        }
    }
}

struct Pane {
    term: Arc<FairMutex<Term<Proxy>>>,
    tx: EventLoopSender,
    size: (usize, usize),
    title: Arc<Mutex<String>>,
    dead: Arc<AtomicBool>,
}

impl Pane {
    fn spawn(ctx: &egui::Context) -> std::io::Result<Pane> {
        let (cols, rows) = (100, 30);
        let tx = Arc::new(OnceLock::new());
        let title = Arc::new(Mutex::new(String::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let proxy = Proxy { ctx: ctx.clone(), tx: tx.clone(), title: title.clone(), dead: dead.clone() };

        // Launched from Finder the cwd is "/"; start in $HOME instead.
        let mut opts = tty::Options::default();
        if std::env::current_dir().is_ok_and(|d| d.as_os_str() == "/") {
            opts.working_directory = std::env::var_os("HOME").map(Into::into);
        }
        tty::setup_env();
        let size = WindowSize { num_cols: cols as u16, num_lines: rows as u16, cell_width: 1, cell_height: 1 };
        let pty = tty::new(&opts, size, 0)?;
        let term = Arc::new(FairMutex::new(Term::new(Config { default_cursor_style: cfg().cursor, ..Config::default() }, &Size { cols, rows }, proxy.clone())));
        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)?;
        let sender = event_loop.channel();
        let _ = tx.set(sender.clone());
        event_loop.spawn();
        Ok(Pane { term, tx: sender, size: (cols, rows), title, dead })
    }

    /// Draws the pane; returns true if it was clicked (wants focus).
    fn show(&mut self, ui: &mut egui::Ui, rect: Rect, font: &FontId, cw: f32, ch: f32, focused: bool, id: usize) -> bool {
        let (cols, rows) = (((rect.width() / cw) as usize).max(2), ((rect.height() / ch) as usize).max(1));
        let mut term = self.term.lock();
        if self.size != (cols, rows) {
            self.size = (cols, rows);
            term.resize(Size { cols, rows });
            let _ = self.tx.send(Msg::Resize(WindowSize {
                num_cols: cols as u16,
                num_lines: rows as u16,
                cell_width: cw as u16,
                cell_height: ch as u16,
            }));
        }

        let resp = ui.interact(rect, ui.scope_id().with(("pane", id)), egui::Sense::CLICK | egui::Sense::DRAG);
        let want_focus = resp.clicked() || resp.drag_started();
        let offset = term.grid().display_offset() as i32;
        let hit = |pos: Pos2| {
            let (x, y) = ((pos.x - rect.left()) / cw, (pos.y - rect.top()) / ch);
            let col = (x.max(0.0) as usize).min(cols - 1);
            let line = (y.max(0.0) as i32).min(rows as i32 - 1) - offset;
            let side = if x.fract() < 0.5 { Side::Left } else { Side::Right };
            (Point::new(Line(line), Column(col)), side)
        };
        if let Some(pos) = resp.interact_pointer_pos() {
            let (pt, side) = hit(pos);
            if resp.drag_started() {
                term.selection = Some(Selection::new(SelectionType::Simple, pt, side));
            } else if resp.dragged() {
                if let Some(s) = term.selection.as_mut() { s.update(pt, side); }
            }
        }
        let cmd = ui.input(|i| i.modifiers.command);
        if cmd && resp.hover_pos().is_some_and(|p| url_at(&term, hit(p).0).is_some()) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            match resp.interact_pointer_pos().and_then(|p| url_at(&term, hit(p).0)).filter(|_| cmd) {
                Some(url) => open_url(&url),
                None => term.selection = None,
            }
        }

        let mut copy = false;
        let app_cursor = term.mode().contains(TermMode::APP_CURSOR);
        if focused { ui.input(|i| {
            let send = |b: Vec<u8>| { let _ = self.tx.send(Msg::Input(b.into())); };
            for e in &i.events {
                match e {
                    egui::Event::Text(t) => send(t.clone().into_bytes()),
                    egui::Event::Paste(t) => send(t.clone().into_bytes()),
                    egui::Event::Copy => copy = true,
                    egui::Event::Key { key, pressed: true, modifiers: m, .. } => {
                        // macOS editing shortcuts: Cmd = line, Alt = word.
                        let b: Option<&[u8]> = match (key, m.command, m.alt) {
                            (Key::Backspace, true, _) => Some(b"\x15"), // kill line (Ctrl+U)
                            (Key::ArrowLeft, true, _) => Some(b"\x01"),
                            (Key::ArrowRight, true, _) => Some(b"\x05"),
                            (Key::Backspace, _, true) => Some(b"\x17"), // kill word (Ctrl+W)
                            (Key::ArrowLeft, _, true) => Some(b"\x1bb"),
                            (Key::ArrowRight, _, true) => Some(b"\x1bf"),
                            _ => None,
                        };
                        if let Some(b) = b {
                            send(b.to_vec());
                        } else if !m.command {
                            if let Some(b) = key_bytes(*key, m.ctrl, app_cursor) { send(b) }
                        }
                    }
                    _ => {}
                }
            }
            let lines = (i.smooth_scroll_delta.y / ch).round() as i32;
            if lines != 0 { term.scroll_display(Scroll::Delta(lines)); }
        }); }
        if copy {
            if let Some(s) = term.selection_to_string() { ui.ctx().copy_text(s); }
        }

        // One galley per run of same-style cells (egui caches them); wide chars get their own run to stay grid-aligned.
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, cfg().bg);
        let content = term.renderable_content();
        let off = content.display_offset as i32;
        let at = |line: i32, col: usize| Pos2::new(rect.left() + col as f32 * cw, rect.top() + (line + off) as f32 * ch);
        let flush = |row: &mut Row| {
            for (s, e, c) in row.bgs.drain(..) {
                p.rect_filled(Rect::from_min_size(at(row.line, s), Vec2::new((e - s) as f32 * cw, ch)), 0.0, c);
            }
            for (col, job) in row.items.drain(..) {
                p.galley(at(row.line, col), p.layout_job(job), cfg().fg);
            }
        };
        let mut row = Row { line: i32::MIN, next: usize::MAX, items: vec![], bgs: vec![] };
        for cell in content.display_iter {
            let f = cell.flags;
            if f.contains(Flags::WIDE_CHAR_SPACER) { continue; }
            if cell.point.line.0 != row.line {
                flush(&mut row);
                row.line = cell.point.line.0;
            }
            let (mut fg, mut bg) = (rgb(cell.fg, cfg().fg), rgb(cell.bg, cfg().bg));
            let selected = content.selection.is_some_and(|s| s.contains(cell.point));
            if f.contains(Flags::INVERSE) != selected { std::mem::swap(&mut fg, &mut bg); }
            if f.contains(Flags::BOLD) { fg = fg.lerp_to_gamma(Color32::WHITE, 0.25); } // no bold face: brighten
            if f.contains(Flags::DIM) { fg = fg.gamma_multiply(0.6); }

            let (col, wide) = (cell.point.column.0, f.contains(Flags::WIDE_CHAR));
            let w = if wide { 2 } else { 1 };
            if bg != cfg().bg {
                match row.bgs.last_mut() {
                    Some((_, e, c)) if *e == col && *c == bg => *e += w,
                    _ => row.bgs.push((col, col + w, bg)),
                }
            }
            let fmt = egui::TextFormat {
                font_id: font.clone(),
                color: fg,
                italics: f.contains(Flags::ITALIC),
                underline: if f.intersects(Flags::ALL_UNDERLINES) { egui::Stroke::new(1.0, fg) } else { egui::Stroke::NONE },
                strikethrough: if f.contains(Flags::STRIKEOUT) { egui::Stroke::new(1.0, fg) } else { egui::Stroke::NONE },
                ..Default::default()
            };
            if wide || row.next != col {
                row.items.push((col, egui::text::LayoutJob::default()));
            }
            let job = &mut row.items.last_mut().unwrap().1;
            let c = if f.contains(Flags::HIDDEN) { ' ' } else { cell.c };
            match job.sections.last_mut() {
                Some(sec) if sec.format == fmt => {
                    job.text.push(c);
                    sec.byte_range.end = egui::text::ByteIndex(job.text.len());
                }
                _ => job.append(c.encode_utf8(&mut [0; 4]), 0.0, fmt),
            }
            row.next = if wide { usize::MAX } else { col + 1 };
        }
        flush(&mut row);
        let style = term.cursor_style();
        let blinking = style.blinking && focused;
        if blinking { ui.ctx().request_repaint_after(std::time::Duration::from_millis(500)); }
        if !blinking || ui.input(|i| i.time) % 1.0 < 0.5 {
            let pos = at(content.cursor.point.line.0, content.cursor.point.column.0);
            let (rect, alpha) = match content.cursor.shape {
                CursorShape::Hidden => (None, 0.0),
                CursorShape::Beam => (Some(Rect::from_min_size(pos, Vec2::new(2.0, ch))), 0.9),
                CursorShape::Underline => (Some(Rect::from_min_size(Pos2::new(pos.x, pos.y + ch - 2.0), Vec2::new(cw, 2.0))), 0.9),
                _ => (Some(Rect::from_min_size(pos, Vec2::new(cw, ch))), if focused { 0.5 } else { 0.15 }),
            };
            if let Some(r) = rect { p.rect_filled(r, 0.0, cfg().fg.gamma_multiply(alpha)); }
        }
        want_focus
    }
}

struct Row {
    line: i32,
    next: usize,
    items: Vec<(usize, egui::text::LayoutJob)>,
    bgs: Vec<(usize, usize, Color32)>,
}

// ponytail: one split direction per tab (the latest split decides), equal sizes; use a tree for mixed layouts.
struct Tab {
    panes: Vec<Pane>,
    focus: usize,
    stacked: bool,
}

impl Tab {
    fn new(pane: Pane) -> Tab {
        Tab { panes: vec![pane], focus: 0, stacked: false }
    }
}

/// The http(s) URL under `pt`, if any (whitespace-delimited token on the visible row).
fn url_at(term: &Term<Proxy>, pt: Point) -> Option<String> {
    let grid = term.grid();
    let row: Vec<char> = (0..grid.columns()).map(|c| grid[pt.line][Column(c)].c).collect();
    let i = pt.column.0.min(row.len() - 1);
    if row[i].is_whitespace() { return None; }
    let s = (0..=i).rev().take_while(|&k| !row[k].is_whitespace()).last()?;
    let e = (i..row.len()).take_while(|&k| !row[k].is_whitespace()).last()?;
    let tok: String = row[s..=e].iter().collect();
    let tok = tok.trim_start_matches(['(', '<', '[', '"', '\'']).trim_end_matches(['.', ',', ';', ':', ')', '>', ']', '}', '"', '\'']);
    (tok.starts_with("http://") || tok.starts_with("https://")).then(|| tok.to_string())
}

fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

fn rgb(c: Color, default: Color32) -> Color32 {
    let hex = |h: u32| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8);
    match c {
        Color::Spec(c) => Color32::from_rgb(c.r, c.g, c.b),
        Color::Named(n) if (n as usize) < 16 => hex(ANSI[n as usize]),
        Color::Named(NamedColor::Background) => cfg().bg,
        Color::Named(_) => default,
        Color::Indexed(i) if i < 16 => hex(ANSI[i as usize]),
        Color::Indexed(i) if i >= 232 => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
        Color::Indexed(i) => {
            let (i, lvl) = (i - 16, |n: u8| if n == 0 { 0 } else { 55 + 40 * n });
            Color32::from_rgb(lvl(i / 36), lvl(i / 6 % 6), lvl(i % 6))
        }
    }
}

fn key_bytes(key: Key, ctrl: bool, app_cursor: bool) -> Option<Vec<u8>> {
    let csi = |c: char| format!("\x1b{}{c}", if app_cursor { 'O' } else { '[' }).into_bytes();
    Some(match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => vec![0x7f],
        Key::Tab => b"\t".to_vec(),
        Key::Escape => vec![0x1b],
        Key::ArrowUp => csi('A'),
        Key::ArrowDown => csi('B'),
        Key::ArrowRight => csi('C'),
        Key::ArrowLeft => csi('D'),
        Key::Home => csi('H'),
        Key::End => csi('F'),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        k if ctrl => {
            let name = k.name();
            let c = name.chars().next().filter(|_| name.len() == 1)?.to_ascii_lowercase();
            if !c.is_ascii_lowercase() { return None; }
            vec![c as u8 - b'a' + 1]
        }
        _ => return None,
    })
}

struct App {
    tabs: Vec<Tab>,
    active: usize,
    title: String,
}

impl App {
    fn new_tab(&mut self, ctx: &egui::Context) {
        if let Ok(p) = Pane::spawn(ctx) {
            self.tabs.push(Tab::new(p));
            self.active = self.tabs.len() - 1;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        for t in &mut self.tabs {
            t.panes.retain(|p| !p.dead.load(Ordering::Relaxed));
            t.focus = t.focus.min(t.panes.len().saturating_sub(1));
        }
        self.tabs.retain(|t| !t.panes.is_empty());
        if self.tabs.is_empty() {
            ui.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.active = self.active.min(self.tabs.len() - 1);

        // Cmd+T new tab, Cmd+W close pane, Cmd+D split (+Shift: stacked), Cmd+[ / ] focus, Cmd+1..9 switch tab.
        let (mut new, mut close, mut goto, mut split, mut step) = (false, false, None, None, 0isize);
        ui.input(|i| {
            if !i.modifiers.command { return; }
            new = i.key_pressed(Key::T);
            close = i.key_pressed(Key::W);
            goto = NUMS.iter().position(|k| i.key_pressed(*k));
            split = i.key_pressed(Key::D).then_some(i.modifiers.shift);
            step = i.key_pressed(Key::CloseBracket) as isize - i.key_pressed(Key::OpenBracket) as isize;
        });
        if new { self.new_tab(ui.ctx()); }
        if close {
            let t = &mut self.tabs[self.active];
            t.panes.remove(t.focus);
            if t.panes.is_empty() { self.tabs.remove(self.active); }
            if self.tabs.is_empty() { return; }
            self.active = self.active.min(self.tabs.len() - 1);
        }
        if let Some(n) = goto.filter(|n| *n < self.tabs.len()) { self.active = n; }
        if let Some(stacked) = split {
            if let Ok(p) = Pane::spawn(ui.ctx()) {
                let t = &mut self.tabs[self.active];
                t.stacked = stacked;
                t.panes.push(p);
                t.focus = t.panes.len() - 1;
            }
        }
        let t = &mut self.tabs[self.active];
        t.focus = t.focus.min(t.panes.len() - 1);
        t.focus = (t.focus as isize + step).rem_euclid(t.panes.len() as isize) as usize;

        let focused = &t.panes[t.focus];
        let title = focused.title.lock().unwrap().clone();
        let title = if title.is_empty() { "own-terminal".to_string() } else { title };
        if title != self.title {
            ui.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        let font = FontId::monospace(cfg().font_size);
        let (cw, ch) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let full = ui.available_rect_before_wrap();
        let (bar, body) = (
            Rect::from_min_size(full.min, Vec2::new(full.width(), TAB_H)),
            Rect::from_min_max(Pos2::new(full.left(), full.top() + TAB_H), full.max),
        );

        // Tab bar: one rect per tab plus a "+" button.
        let p = ui.painter_at(bar);
        p.rect_filled(bar, 0.0, cfg().bar);
        let w = (bar.width() / (self.tabs.len() as f32 + 1.0)).min(180.0);
        let mut spawn = false;
        for (n, tab) in self.tabs.iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(bar.left() + n as f32 * w, bar.top()), Vec2::new(w, TAB_H));
            if ui.interact(r, ui.scope_id().with(("tab", n)), egui::Sense::CLICK).clicked() {
                self.active = n;
            }
            if n == self.active { p.rect_filled(r, 0.0, cfg().bg); }
            let title = tab.panes[tab.focus.min(tab.panes.len() - 1)].title.lock().unwrap().clone();
            let title = if title.is_empty() { format!("{}", n + 1) } else { title };
            p.with_clip_rect(r.shrink2(Vec2::new(6.0, 0.0))).text(
                Pos2::new(r.left() + 8.0, r.center().y),
                egui::Align2::LEFT_CENTER,
                title,
                FontId::proportional(12.0),
                if n == self.active { cfg().fg } else { cfg().fg.gamma_multiply(0.55) },
            );
        }
        let plus = Rect::from_min_size(Pos2::new(bar.left() + self.tabs.len() as f32 * w, bar.top()), Vec2::splat(TAB_H));
        if ui.interact(plus, ui.scope_id().with("plus"), egui::Sense::CLICK).clicked() { spawn = true; }
        p.text(plus.center(), egui::Align2::CENTER_CENTER, "+", FontId::proportional(14.0), cfg().fg);
        if spawn { self.new_tab(ui.ctx()); }

        // Panes split the body evenly; the gap between them shows the bar color as a divider.
        ui.painter_at(body).rect_filled(body, 0.0, cfg().bar);
        let t = &mut self.tabs[self.active];
        let n = t.panes.len() as f32;
        let mut new_focus = None;
        for (k, pane) in t.panes.iter_mut().enumerate() {
            let r = if t.stacked {
                let h = body.height() / n;
                Rect::from_min_size(Pos2::new(body.left(), body.top() + k as f32 * h), Vec2::new(body.width(), h - 1.0))
            } else {
                let wd = body.width() / n;
                Rect::from_min_size(Pos2::new(body.left() + k as f32 * wd, body.top()), Vec2::new(wd - 1.0, body.height()))
            };
            if pane.show(ui, r, &font, cw, ch, k == t.focus, k) { new_focus = Some(k); }
        }
        if let Some(k) = new_focus { t.focus = k; }
    }
}

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("own-terminal", opts, Box::new(|cc| {
        load_font(&cc.egui_ctx);
        Ok(Box::new(App { tabs: vec![Tab::new(Pane::spawn(&cc.egui_ctx)?)], active: 0, title: String::new() }))
    }))
}

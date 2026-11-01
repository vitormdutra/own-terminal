use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;

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
// Keys: font_size, font_path (.ttf/.otf), fg, bg (#rrggbb), cursor (block|bar|underline), cursor_blink (true|false),
// notify_after (seconds a command must run before a notification when the window is in background; 0 = off).
struct Settings {
    font_size: f32,
    font_path: Option<String>,
    fg: Color32,
    bg: Color32,
    bar: Color32,
    cursor: CursorStyle,
    notify_after: u64,
}

fn load() -> Settings {
    {
        let mut s = Settings {
            font_size: 14.0,
            font_path: None,
            fg: Color32::from_rgb(0xd8, 0xd8, 0xd8),
            bg: Color32::from_rgb(0x18, 0x18, 0x18),
            bar: Color32::BLACK,
            cursor: CursorStyle { shape: CursorShape::Block, blinking: false },
            notify_after: 10,
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
                "notify_after" => s.notify_after = v.parse().unwrap_or(s.notify_after),
                _ => {}
            }
        }
        s.bar = s.bg.gamma_multiply(0.6);
        s
    }
}

// Settings are leaked on (re)load so cfg() can hand out plain references; a reload is rare and tiny.
static CFG: AtomicPtr<Settings> = AtomicPtr::new(std::ptr::null_mut());

fn cfg() -> &'static Settings {
    let mut p = CFG.load(Ordering::Acquire);
    if p.is_null() {
        reload();
        p = CFG.load(Ordering::Acquire);
    }
    unsafe { &*p }
}

fn reload() {
    CFG.store(Box::leak(Box::new(load())), Ordering::Release);
}

/// Working directory of a process, if the OS lets us read it.
#[cfg(target_os = "linux")]
fn cwd_of(pid: i32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
fn cwd_of(pid: i32) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    unsafe {
        let mut info: libc::proc_vnodepathinfo = std::mem::zeroed();
        let size = std::mem::size_of_val(&info) as libc::c_int;
        let n = libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, &mut info as *mut _ as *mut libc::c_void, size);
        if n != size { return None; }
        let bytes = std::slice::from_raw_parts(info.pvi_cdir.vip_path.as_ptr() as *const u8, 1024);
        let end = bytes.iter().position(|&b| b == 0)?;
        Some(std::ffi::OsStr::from_bytes(&bytes[..end]).into())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cwd_of(_pid: i32) -> Option<std::path::PathBuf> {
    None
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
    // Foreground-process tracking (unix): the foreground process group seen when idle is the shell's;
    // anything else in front means a command is running. (Not child.id(): macOS wraps the shell in `login`.)
    fd: i32,
    shell: i32,
    born: Instant,
    busy_since: Option<Instant>,
    // Scrollback search: (grid line, column) of each match, all `match_len` cells wide.
    matches: Vec<(i32, usize)>,
    match_len: usize,
    match_idx: usize,
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

struct View<'a> {
    font: &'a FontId,
    cw: f32,
    ch: f32,
    /// Search bar is open: highlight matches.
    hl: bool,
    /// Search box has keyboard focus: the terminal must not receive input.
    block: bool,
    /// More than one pane in the tab: mark the focused one.
    multi: bool,
}

impl Pane {
    /// `cwd`: where the shell starts (None = inherit, or $HOME when launched from Finder).
    fn spawn(ctx: &egui::Context, cwd: Option<std::path::PathBuf>) -> std::io::Result<Pane> {
        let (cols, rows) = (100, 30);
        let tx = Arc::new(OnceLock::new());
        let title = Arc::new(Mutex::new(String::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let proxy = Proxy { ctx: ctx.clone(), tx: tx.clone(), title: title.clone(), dead: dead.clone() };

        // Launched from Finder the cwd is "/"; start in $HOME instead.
        let mut opts = tty::Options::default();
        opts.working_directory = cwd;
        if opts.working_directory.is_none() && std::env::current_dir().is_ok_and(|d| d.as_os_str() == "/") {
            opts.working_directory = std::env::var_os("HOME").map(Into::into);
        }
        tty::setup_env();
        let size = WindowSize { num_cols: cols as u16, num_lines: rows as u16, cell_width: 1, cell_height: 1 };
        let pty = tty::new(&opts, size, 0)?;
        #[cfg(unix)]
        let fd = pty.file().as_raw_fd();
        #[cfg(not(unix))]
        let fd = -1;
        let term = Arc::new(FairMutex::new(Term::new(Config { default_cursor_style: cfg().cursor, ..Config::default() }, &Size { cols, rows }, proxy.clone())));
        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)?;
        let sender = event_loop.channel();
        let _ = tx.set(sender.clone());
        event_loop.spawn();
        Ok(Pane {
            term, tx: sender, size: (cols, rows), title, dead, fd, shell: -1, born: Instant::now(),
            busy_since: None, matches: vec![], match_len: 0, match_idx: 0,
        })
    }

    /// The shell's current directory (the idle foreground group leader is the shell itself).
    fn cwd(&self) -> Option<std::path::PathBuf> {
        if self.shell > 0 { cwd_of(self.shell) } else { None }
    }

    /// Call often. Returns the run time in seconds when a foreground command that ran at least `min` seconds just finished.
    fn poll(&mut self, min: u64) -> Option<u64> {
        #[cfg(unix)]
        {
            if self.dead.load(Ordering::Relaxed) { return None; }
            let fg = unsafe { libc::tcgetpgrp(self.fd) };
            if fg <= 0 { return None; }
            if self.shell < 0 {
                // ponytail: assumes the pane is idle 2s after opening; a command started sooner disables its notifications.
                if self.born.elapsed() >= Duration::from_secs(2) { self.shell = fg; }
                return None;
            }
            let busy = fg != self.shell;
            match (busy, self.busy_since) {
                (true, None) => self.busy_since = Some(Instant::now()),
                (false, Some(t)) => {
                    self.busy_since = None;
                    let secs = t.elapsed().as_secs();
                    if min > 0 && secs >= min { return Some(secs); }
                }
                _ => {}
            }
        }
        let _ = min;
        None
    }

    /// Case-insensitive search over scrollback + screen; selects the newest match.
    fn search(&mut self, query: &str) {
        self.matches.clear();
        let q: Vec<char> = query.to_lowercase().chars().collect();
        self.match_len = q.len();
        if !q.is_empty() {
            let term = self.term.lock();
            let g = term.grid();
            for l in g.topmost_line().0..=g.bottommost_line().0 {
                let row: Vec<char> = (0..g.columns())
                    .map(|c| g[Line(l)][Column(c)].c.to_lowercase().next().unwrap_or(' '))
                    .collect();
                let mut i = 0;
                while i + q.len() <= row.len() {
                    if row[i..i + q.len()] == q[..] {
                        self.matches.push((l, i));
                        i += q.len();
                    } else {
                        i += 1;
                    }
                }
            }
        }
        self.match_idx = self.matches.len().saturating_sub(1);
    }

    /// Moves to the match `step` away from the current one and scrolls it into view.
    fn jump(&mut self, step: isize) {
        if self.matches.is_empty() { return; }
        let n = self.matches.len() as isize;
        self.match_idx = (self.match_idx as isize + step).rem_euclid(n) as usize;
        let line = self.matches[self.match_idx].0;
        let mut term = self.term.lock();
        term.scroll_display(Scroll::Bottom);
        if line < 0 { term.scroll_display(Scroll::Delta(-line + self.size.1 as i32 / 2)); }
    }

    /// Draws the pane; returns true if it was clicked (wants focus).
    fn show(&mut self, ui: &mut egui::Ui, rect: Rect, v: &View, focused: bool, id: usize) -> bool {
        let (font, cw, ch) = (v.font, v.cw, v.ch);
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

        if let Some(pos) = resp.interact_pointer_pos() {
            if resp.triple_clicked() || resp.double_clicked() {
                let (pt, side) = hit(pos);
                let kind = if resp.triple_clicked() { SelectionType::Lines } else { SelectionType::Semantic };
                term.selection = Some(Selection::new(kind, pt, side));
            }
        }

        let mut copy = false;
        let app_cursor = term.mode().contains(TermMode::APP_CURSOR);
        let bracketed = term.mode().contains(TermMode::BRACKETED_PASTE);
        if focused && !v.block { ui.input(|i| {
            let typed = std::cell::Cell::new(false);
            let send = |b: Vec<u8>| {
                typed.set(true);
                let _ = self.tx.send(Msg::Input(b.into()));
            };
            for e in &i.events {
                match e {
                    egui::Event::Text(t) => send(t.clone().into_bytes()),
                    egui::Event::Paste(t) => send(paste_bytes(t, bracketed)),
                    egui::Event::Copy => copy = true,
                    egui::Event::Key { key, pressed: true, modifiers: m, .. } => {
                        // macOS editing shortcuts: Cmd = line, Alt = word.
                        let b: Option<&[u8]> = match (key, m.command, m.alt) {
                            (Key::Backspace, true, _) => Some(b"\x15"), // kill line (Ctrl+U)
                            (Key::ArrowLeft, true, false) => Some(b"\x01"),
                            (Key::ArrowRight, true, false) => Some(b"\x05"),
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
            // Like any terminal: typing (or Ctrl+C...) jumps back to the prompt if you were scrolled up in the history.
            if typed.get() { term.scroll_display(Scroll::Bottom); }
            let lines = (i.smooth_scroll_delta.y / ch).round() as i32;
            if lines != 0 { term.scroll_display(Scroll::Delta(lines)); }
        }); }
        if copy {
            if let Some(s) = term.selection_to_string() { ui.ctx().copy_text(s); }
        }

        let mut hits: HashMap<(i32, usize), bool> = HashMap::new(); // cell -> is the current match
        if v.hl && self.match_len > 0 {
            for (k, &(l, c)) in self.matches.iter().enumerate() {
                if l >= -offset && l < -offset + rows as i32 {
                    for x in 0..self.match_len { hits.insert((l, c + x), k == self.match_idx); }
                }
            }
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
            if let Some(&cur) = hits.get(&(cell.point.line.0, cell.point.column.0)) {
                bg = if cur { Color32::from_rgb(0xf0, 0x8c, 0x3c) } else { Color32::from_rgb(0xf7, 0xca, 0x88) };
                fg = Color32::BLACK;
            }

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
        if v.multi && focused {
            let c = cfg().fg.gamma_multiply(0.35);
            for r in [
                Rect::from_min_size(rect.min, Vec2::new(rect.width(), 1.0)),
                Rect::from_min_size(Pos2::new(rect.left(), rect.bottom() - 1.0), Vec2::new(rect.width(), 1.0)),
                Rect::from_min_size(rect.min, Vec2::new(1.0, rect.height())),
                Rect::from_min_size(Pos2::new(rect.right() - 1.0, rect.top()), Vec2::new(1.0, rect.height())),
            ] {
                p.rect_filled(r, 0.0, c);
            }
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

/// Layout tree: leaves are terminals, splits hold two children and a draggable ratio.
enum Node {
    Empty,
    Leaf(Pane),
    Split { stacked: bool, ratio: f32, a: Box<Node>, b: Box<Node> },
}

/// Bookkeeping while drawing a tree.
struct Walk {
    idx: usize,
    focus: usize,
    new_focus: Option<usize>,
    div: usize,
    rects: Vec<Rect>,
}

const DIVIDER: f32 = 5.0;

impl Node {
    fn panes<'a>(&'a self, out: &mut Vec<&'a Pane>) {
        match self {
            Node::Empty => {}
            Node::Leaf(p) => out.push(p),
            Node::Split { a, b, .. } => { a.panes(out); b.panes(out); }
        }
    }

    fn panes_mut<'a>(&'a mut self, out: &mut Vec<&'a mut Pane>) {
        match self {
            Node::Empty => {}
            Node::Leaf(p) => out.push(p),
            Node::Split { a, b, .. } => { a.panes_mut(out); b.panes_mut(out); }
        }
    }

    fn count(&self) -> usize {
        let mut v = Vec::new();
        self.panes(&mut v);
        v.len()
    }

    /// Drops dead panes (a split with one dead child collapses into the other). True if nothing is left.
    fn prune(&mut self) -> bool {
        match self {
            Node::Empty => true,
            Node::Leaf(p) => p.dead.load(Ordering::Relaxed),
            Node::Split { a, b, .. } => {
                let (da, db) = (a.prune(), b.prune());
                if da && db { return true; }
                if da { *self = std::mem::replace(&mut **b, Node::Empty); }
                else if db { *self = std::mem::replace(&mut **a, Node::Empty); }
                false
            }
        }
    }

    /// Splits the `idx`-th leaf (DFS order), putting `new` after it.
    fn split(&mut self, idx: &mut usize, new: &mut Option<Pane>, stacked: bool) -> bool {
        match self {
            Node::Empty => false,
            Node::Leaf(_) if *idx == 0 => {
                let old = std::mem::replace(self, Node::Empty);
                let new = Node::Leaf(new.take().expect("pane"));
                *self = Node::Split { stacked, ratio: 0.5, a: Box::new(old), b: Box::new(new) };
                true
            }
            Node::Leaf(_) => { *idx -= 1; false }
            Node::Split { a, b, .. } => a.split(idx, new, stacked) || b.split(idx, new, stacked),
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, rect: Rect, v: &View, w: &mut Walk) {
        match self {
            Node::Empty => {}
            Node::Leaf(p) => {
                let k = w.idx;
                w.idx += 1;
                w.rects.push(rect);
                if p.show(ui, rect, v, k == w.focus, k) { w.new_focus = Some(k); }
            }
            Node::Split { stacked, ratio, a, b } => {
                let total = if *stacked { rect.height() } else { rect.width() };
                let at = (total * *ratio).round();
                let (ra, rb, div) = if *stacked {
                    let y = rect.top() + at;
                    (
                        Rect::from_min_max(rect.min, Pos2::new(rect.right(), y - DIVIDER / 2.0)),
                        Rect::from_min_max(Pos2::new(rect.left(), y + DIVIDER / 2.0), rect.max),
                        Rect::from_min_max(Pos2::new(rect.left(), y - DIVIDER / 2.0), Pos2::new(rect.right(), y + DIVIDER / 2.0)),
                    )
                } else {
                    let x = rect.left() + at;
                    (
                        Rect::from_min_max(rect.min, Pos2::new(x - DIVIDER / 2.0, rect.bottom())),
                        Rect::from_min_max(Pos2::new(x + DIVIDER / 2.0, rect.top()), rect.max),
                        Rect::from_min_max(Pos2::new(x - DIVIDER / 2.0, rect.top()), Pos2::new(x + DIVIDER / 2.0, rect.bottom())),
                    )
                };
                let r = ui.interact(div, ui.scope_id().with(("div", w.div)), egui::Sense::DRAG);
                w.div += 1;
                if r.hovered() || r.dragged() {
                    ui.ctx().set_cursor_icon(if *stacked { egui::CursorIcon::ResizeVertical } else { egui::CursorIcon::ResizeHorizontal });
                }
                if let (true, Some(p)) = (r.dragged(), r.interact_pointer_pos()) {
                    let pos = if *stacked { p.y - rect.top() } else { p.x - rect.left() };
                    *ratio = (pos / total).clamp(0.1, 0.9);
                }
                ui.painter_at(div).rect_filled(div, 0.0, cfg().bar);
                a.show(ui, ra, v, w);
                b.show(ui, rb, v, w);
            }
        }
    }
}

struct Tab {
    root: Node,
    focus: usize,
}

impl Tab {
    fn new(pane: Pane) -> Tab {
        Tab { root: Node::Leaf(pane), focus: 0 }
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

/// Runs a helper process without blocking the UI (and reaps it).
fn detach(mut cmd: std::process::Command) {
    std::thread::spawn(move || { let _ = cmd.status(); });
}

fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    let mut c = std::process::Command::new(opener);
    c.arg(url);
    detach(c);
}

/// Desktop notification. The text goes in as an argument, never into a script, since it can come from the terminal.
fn notify(body: &str) {
    #[cfg(target_os = "macos")]
    {
        let mut c = std::process::Command::new("osascript");
        c.args(["-e", "on run argv", "-e", "display notification (item 1 of argv) with title \"own-terminal\"", "-e", "end run", body]);
        detach(c);
    }
    #[cfg(target_os = "linux")]
    {
        let mut c = std::process::Command::new("notify-send");
        c.args(["own-terminal", body]);
        detach(c);
    }
    let _ = body;
}

/// Clipboard text as terminal input: newlines become CR, control characters (ESC especially) are dropped,
/// and when the shell asked for bracketed paste the text is fenced so it is inserted, not executed.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let body: String = text
        .replace("\r\n", "\r")
        .replace('\n', "\r")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\r' | '\t'))
        .collect();
    if bracketed { format!("\x1b[200~{body}\x1b[201~").into_bytes() } else { body.into_bytes() }
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

const SEARCH_H: f32 = 28.0;

/// The pane next to `rects[from]` in direction (dx, dy): its center lies that way and it overlaps on the other axis.
fn nav(rects: &[Rect], from: usize, dx: i32, dy: i32) -> Option<usize> {
    let c = *rects.get(from)?;
    let dist = |r: &Rect| (r.center() - c.center()).length();
    rects
        .iter()
        .enumerate()
        .filter(|(j, r)| {
            let d = r.center() - c.center();
            *j != from
                && if dx != 0 {
                    d.x * dx as f32 > 0.0 && r.top() < c.bottom() && r.bottom() > c.top()
                } else {
                    d.y * dy as f32 > 0.0 && r.left() < c.right() && r.right() > c.left()
                }
        })
        .min_by(|a, b| dist(a.1).total_cmp(&dist(b.1)))
        .map(|(j, _)| j)
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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
        // Cmd+= / - / 0 zoom, Cmd+F search.
        let mut k = Keys::default();
        ui.input(|i| {
            k.esc = i.key_pressed(Key::Escape);
            if !i.modifiers.command { return; }
            k.new = i.key_pressed(Key::T);
            k.close = i.key_pressed(Key::W);
            k.goto = NUMS.iter().position(|n| i.key_pressed(*n));
            k.split = i.key_pressed(Key::D).then_some(i.modifiers.shift);
            k.step = i.key_pressed(Key::CloseBracket) as isize - i.key_pressed(Key::OpenBracket) as isize;
            k.zoom = (i.key_pressed(Key::Equals) || i.key_pressed(Key::Plus)) as i32 - i.key_pressed(Key::Minus) as i32;
            k.reset = i.key_pressed(Key::Num0);
            k.find = i.key_pressed(Key::F);
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
            }
        }
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
        for (n, tab) in self.tabs.iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(bar.left() + n as f32 * w, bar.top()), Vec2::new(w, TAB_H));
            if ui.interact(r, ui.scope_id().with(("tab", n)), egui::Sense::CLICK).clicked() {
                self.active = n;
            }
            if n == self.active { p.rect_filled(r, 0.0, cfg().bg); }
            let mut ps = Vec::new();
            tab.root.panes(&mut ps);
            let title = ps[tab.focus.min(ps.len() - 1)].title.lock().unwrap().clone();
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
        if self.active >= self.tabs.len() { return; }

        // Panes (and their draggable dividers) fill the body.
        ui.painter_at(body).rect_filled(body, 0.0, cfg().bar);
        let view = View { font: &font, cw, ch, hl: self.search.is_some(), block: self.search_focus, multi };
        let t = &mut self.tabs[self.active];
        let mut walk = Walk { idx: 0, focus: t.focus, new_focus: None, div: 0, rects: vec![] };
        t.root.show(ui, body, &view, &mut walk);
        if let Some(f) = walk.new_focus { t.focus = f; }
        self.rects = walk.rects;

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;

    /// The notification feature hinges on this: the idle shell owns the foreground process group,
    /// and a running command takes it over.
    #[test]
    fn foreground_group_tracks_running_command() {
        let size = WindowSize { num_cols: 80, num_lines: 24, cell_width: 1, cell_height: 1 };
        // Plain sh: a fancy rc file may wait for terminal replies that nobody sends in a test.
        let opts = tty::Options { shell: Some(tty::Shell::new("/bin/sh".into(), vec![])), ..Default::default() };
        let pty = tty::new(&opts, size, 0).unwrap();
        let fd = pty.file().as_raw_fd();
        let fg = || unsafe { libc::tcgetpgrp(fd) };
        let wait = |secs| std::thread::sleep(Duration::from_secs(secs));

        wait(3); // shell startup
        let shell = fg();
        assert!(shell > 0);
        wait(1);
        assert_eq!(fg(), shell, "idle foreground group must be stable");
        let mut f: &std::fs::File = pty.file();
        f.write_all(b"sleep 4\n").unwrap();
        // The shell may still be loading its rc files, so poll instead of fixed sleeps.
        let until = |cond: &dyn Fn() -> bool| (0..100).any(|_| { std::thread::sleep(Duration::from_millis(100)); cond() });
        assert!(until(&|| fg() != shell), "sleep should take the foreground");
        assert!(until(&|| fg() == shell), "shell should be foreground again");
        std::mem::forget(pty); // Pty::drop waits on the child; irrelevant for the test
    }
}

#[cfg(test)]
mod paste_tests {
    use super::paste_bytes;

    #[test]
    fn paste_is_sanitized_and_fenced() {
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(paste_bytes("x\x1b[201~rm", false), b"x[201~rm"); // cannot close the fence early
        assert_eq!(paste_bytes("ls", true), b"\x1b[200~ls\x1b[201~");
    }
}

#[cfg(test)]
mod nav_tests {
    use super::*;

    #[test]
    fn nav_picks_the_neighbour_in_that_direction() {
        // [0 | 1] over [2]
        let r = |x0, y0, x1, y1| Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1));
        let rects = [r(0.0, 0.0, 50.0, 50.0), r(50.0, 0.0, 100.0, 50.0), r(0.0, 50.0, 100.0, 100.0)];
        assert_eq!(nav(&rects, 0, 1, 0), Some(1));
        assert_eq!(nav(&rects, 1, -1, 0), Some(0));
        assert_eq!(nav(&rects, 0, 0, 1), Some(2));
        assert_eq!(nav(&rects, 2, 0, -1), Some(0)); // nearest center wins over pane 1
        assert_eq!(nav(&rects, 0, -1, 0), None);
    }
}

#[cfg(all(test, unix))]
mod cwd_tests {
    use super::cwd_of;

    #[test]
    fn cwd_of_reads_a_process_directory() {
        let me = unsafe { libc::getpid() };
        let want = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(cwd_of(me).unwrap().canonicalize().unwrap(), want);
    }
}

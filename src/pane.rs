use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use eframe::egui::{self, Color32, FontId, Key, Pos2, Rect, Vec2};

use crate::config::*;

/// Working directory of a process, if the OS lets us read it.
#[cfg(target_os = "linux")]
pub fn cwd_of(pid: i32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
pub fn cwd_of(pid: i32) -> Option<std::path::PathBuf> {
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
pub fn cwd_of(_pid: i32) -> Option<std::path::PathBuf> {
    None
}

pub struct Size {
    pub cols: usize,
    pub rows: usize,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize { self.rows }
    fn screen_lines(&self) -> usize { self.rows }
    fn columns(&self) -> usize { self.cols }
}

// zsh only: a ZDOTDIR shim whose .zshenv restores the real ZDOTDIR, sources the user's .zshenv, then
// wraps each command's output in an OSC 8 hyperlink (uri "own:block", unique id) that `Pane::last_output` finds.
// ponytail: zsh only; bash/fish need their own hook (PROMPT_COMMAND / fish_preexec).
const ZSH_HOOK: &str = r#"ZDOTDIR=$OWN_ZDOTDIR; [[ -n $ZDOTDIR ]] || unset ZDOTDIR
unset OWN_ZDOTDIR
[[ -f ${ZDOTDIR:-$HOME}/.zshenv ]] && source ${ZDOTDIR:-$HOME}/.zshenv
if [[ -o interactive ]]; then
  autoload -Uz add-zsh-hook
  _own_pre() { printf '\e]8;id=%s_%s;own:block\a' $$ $((++_own_n)) }
  _own_post() { printf '\e]8;;\a' }
  add-zsh-hook preexec _own_pre
  add-zsh-hook precmd _own_post
fi
"#;

fn zsh_block_hook(opts: &mut tty::Options) {
    if !std::env::var("SHELL").map_or(cfg!(target_os = "macos"), |s| s.ends_with("zsh")) { return; }
    let dir = std::env::temp_dir().join("own-terminal-zsh");
    if std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(".zshenv"), ZSH_HOOK)).is_err() { return; }
    opts.env.insert("OWN_ZDOTDIR".into(), std::env::var("ZDOTDIR").unwrap_or_default());
    opts.env.insert("ZDOTDIR".into(), dir.to_string_lossy().into_owned());
}

fn last_block<T>(term: &Term<T>) -> Option<String> {
    let g = term.grid();
        let cols = g.columns();
    let mut id: Option<String> = None;
    let mut rows: Vec<(String, bool)> = vec![]; // newest first: (text, soft-wrapped into next row)
    let mut blanks: Vec<(String, bool)> = vec![];
    for l in (g.topmost_line().0..=g.bottommost_line().0).rev() {
        let row = &g[Line(l)];
        let mut tag = None;
        let mut s = String::new();
        for c in 0..cols {
            let cell = &row[Column(c)];
            if let Some(h) = cell.hyperlink().filter(|h| h.uri() == "own:block") { tag = Some(h.id().to_string()); }
            if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) { s.push(cell.c); }
        }
        let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        match (tag, &id) {
            (Some(t), Some(i)) if t != *i => break, // previous command's block
            (Some(t), _) => {
                id = Some(t);
                rows.append(&mut blanks);
                rows.push((s, wrapped));
            }
            (None, Some(_)) if !s.trim().is_empty() => break, // prompt/command line above the block
            (None, Some(_)) => blanks.push((s, wrapped)),
            (None, None) => {}
        }
    }
    if rows.is_empty() { return None; }
    let mut out = String::new();
    for (s, wrapped) in rows.iter().rev() {
        if *wrapped { out.push_str(s); } else { out.push_str(s.trim_end()); out.push('\n'); }
    }
    Some(out.trim_end().to_string())
}

// Bridges one tab's terminal events to egui (repaint/title/exit) and back to its pty (query replies).
#[derive(Clone)]
pub struct Proxy {
    pub ctx: egui::Context,
    pub tx: Arc<OnceLock<EventLoopSender>>,
    pub title: Arc<Mutex<String>>,
    pub dead: Arc<AtomicBool>,
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

pub struct Pane {
    pub term: Arc<FairMutex<Term<Proxy>>>,
    pub tx: EventLoopSender,
    pub size: (usize, usize),
    pub title: Arc<Mutex<String>>,
    pub dead: Arc<AtomicBool>,
    // Foreground-process tracking (unix): the foreground process group seen when idle is the shell's;
    // anything else in front means a command is running. (Not child.id(): macOS wraps the shell in `login`.)
    pub fd: i32,
    pub shell: i32,
    pub born: Instant,
    pub busy_since: Option<Instant>,
    // Scrollback search: (grid line, column) of each match, all `match_len` cells wide.
    pub matches: Vec<(i32, usize)>,
    pub match_len: usize,
    pub match_idx: usize,
    // Mouse reporting: the held button's code (with modifier bits) and the last cell reported.
    pub mouse_btn: Option<u8>,
    pub mouse_cell: (usize, usize),
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

pub struct View<'a> {
    pub font: &'a FontId,
    pub cw: f32,
    pub ch: f32,
    /// Search bar is open: highlight matches.
    pub hl: bool,
    /// Search box has keyboard focus: the terminal must not receive input.
    pub block: bool,
    /// More than one pane in the tab: mark the focused one.
    pub multi: bool,
}

impl Pane {
    /// `cwd`: where the shell starts (None = inherit, or $HOME when launched from Finder).
    pub fn spawn(ctx: &egui::Context, cwd: Option<std::path::PathBuf>) -> std::io::Result<Pane> {
        let (cols, rows) = (100, 30);
        let tx = Arc::new(OnceLock::new());
        let title = Arc::new(Mutex::new(String::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let proxy = Proxy { ctx: ctx.clone(), tx: tx.clone(), title: title.clone(), dead: dead.clone() };

        // Launched from Finder the cwd is "/"; start in $HOME instead.
        let mut opts = tty::Options { working_directory: cwd, ..Default::default() };
        if opts.working_directory.is_none() && std::env::current_dir().is_ok_and(|d| d.as_os_str() == "/") {
            opts.working_directory = home_dir();
        }
        tty::setup_env();
        zsh_block_hook(&mut opts);
        let size = WindowSize { num_cols: cols as u16, num_lines: rows as u16, cell_width: 1, cell_height: 1 };
        let pty = tty::new(&opts, size, 0)?;
        #[cfg(unix)]
        let fd = pty.file().as_raw_fd();
        #[cfg(not(unix))]
        let fd = -1;
        let term = Arc::new(FairMutex::new(Term::new(Config { default_cursor_style: cfg().cursor, scrolling_history: cfg().scrollback, ..Config::default() }, &Size { cols, rows }, proxy.clone())));
        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)?;
        let sender = event_loop.channel();
        let _ = tx.set(sender.clone());
        event_loop.spawn();
        Ok(Pane {
            term, tx: sender, size: (cols, rows), title, dead, fd, shell: -1, born: Instant::now(),
            busy_since: None, matches: vec![], match_len: 0, match_idx: 0, mouse_btn: None, mouse_cell: (0, 0),
        })
    }

    /// Output of the newest command (running or finished): the run of cells the shell hook tagged with
    /// an `own:block` hyperlink. Blank lines inside the output carry no tag, so they are kept only when
    /// more tagged output of the same block lies above them.
    pub fn last_output(&self) -> Option<String> { last_block(&self.term.lock()) }

    /// The shell's current directory (the idle foreground group leader is the shell itself).
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        if self.shell > 0 { cwd_of(self.shell) } else { None }
    }

    /// Call often. Returns the run time in seconds when a foreground command that ran at least `min` seconds just finished.
    pub fn poll(&mut self, min: u64) -> Option<u64> {
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
    pub fn search(&mut self, query: &str) {
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

    /// Re-runs the search for new output, keeping the current match (new matches only append at the end).
    pub fn refresh_search(&mut self, query: &str) {
        let i = self.match_idx;
        self.search(query);
        self.match_idx = i.min(self.matches.len().saturating_sub(1));
    }

    /// Moves to the match `step` away from the current one and scrolls it into view.
    pub fn jump(&mut self, step: isize) {
        if self.matches.is_empty() { return; }
        let n = self.matches.len() as isize;
        self.match_idx = (self.match_idx as isize + step).rem_euclid(n) as usize;
        let line = self.matches[self.match_idx].0;
        let mut term = self.term.lock();
        term.scroll_display(Scroll::Bottom);
        if line < 0 { term.scroll_display(Scroll::Delta(-line + self.size.1 as i32 / 2)); }
    }

    /// Draws the pane; returns true if it was clicked (wants focus).
    pub fn show(&mut self, ui: &mut egui::Ui, area: Rect, v: &View, focused: bool, id: usize) -> bool {
        let (font, cw, ch) = (v.font, v.cw, v.ch);
        let rect = area.shrink(cfg().padding); // the character grid; `area` is the whole pane
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

        let resp = ui.interact(area, ui.scope_id().with(("pane", id)), egui::Sense::CLICK | egui::Sense::DRAG);
        let want_focus = resp.clicked() || resp.drag_started();
        let offset = term.grid().display_offset() as i32;
        let hit = |pos: Pos2| {
            let (x, y) = ((pos.x - rect.left()) / cw, (pos.y - rect.top()) / ch);
            let col = (x.max(0.0) as usize).min(cols - 1);
            let line = (y.max(0.0) as i32).min(rows as i32 - 1) - offset;
            let side = if x.fract() < 0.5 { Side::Left } else { Side::Right };
            (Point::new(Line(line), Column(col)), side)
        };
        // Programs like vim and htop can ask for the mouse; Shift always keeps the local selection.
        let mode = *term.mode();
        let mouse = mode.intersects(TermMode::MOUSE_MODE) && !ui.input(|i| i.modifiers.shift);
        let cell_at = |pos: Pos2| {
            (
                (((pos.x - rect.left()) / cw).max(0.0) as usize).min(cols - 1),
                (((pos.y - rect.top()) / ch).max(0.0) as usize).min(rows - 1),
            )
        };
        if let Some(pos) = resp.interact_pointer_pos().filter(|_| !mouse) {
            let (pt, side) = hit(pos);
            if resp.drag_started() {
                term.selection = Some(Selection::new(SelectionType::Simple, pt, side));
            } else if resp.dragged()
                && let Some(s) = term.selection.as_mut() { s.update(pt, side); }
        }
        let cmd = ui.input(|i| i.modifiers.command);
        if cmd && !mouse && resp.hover_pos().is_some_and(|p| url_at(&term, hit(p).0).is_some()) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() && !mouse {
            match resp.interact_pointer_pos().and_then(|p| url_at(&term, hit(p).0)).filter(|_| cmd) {
                Some(url) => open_url(&url),
                None => term.selection = None,
            }
        }

        if let Some(pos) = resp.interact_pointer_pos().filter(|_| !mouse)
            && (resp.triple_clicked() || resp.double_clicked()) {
                let (pt, side) = hit(pos);
                let kind = if resp.triple_clicked() { SelectionType::Lines } else { SelectionType::Semantic };
                term.selection = Some(Selection::new(kind, pt, side));
            }

        let mut copy = false;
        let app_cursor = term.mode().contains(TermMode::APP_CURSOR);
        let bracketed = term.mode().contains(TermMode::BRACKETED_PASTE);
        let sgr = mode.contains(TermMode::SGR_MOUSE);
        let meta = cfg().option_as_meta;

        if mouse {
            ui.input(|i| {
                for e in &i.events {
                    let (code, cell, press) = match e {
                        egui::Event::PointerButton { pos, button, pressed, modifiers: m } if area.contains(*pos) || !*pressed => {
                            let b = match button {
                                egui::PointerButton::Primary => 0,
                                egui::PointerButton::Middle => 1,
                                egui::PointerButton::Secondary => 2,
                                _ => continue,
                            };
                            // A release only matters if we reported the press.
                            if !*pressed && self.mouse_btn.is_none() { continue; }
                            let code = b | (m.shift as u8 * 4) | (m.alt as u8 * 8) | (m.ctrl as u8 * 16);
                            self.mouse_btn = pressed.then_some(code);
                            (code, cell_at(*pos), *pressed)
                        }
                        egui::Event::PointerMoved(pos) => {
                            let cell = cell_at(*pos);
                            let held = self.mouse_btn.is_some() && mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION);
                            let free = self.mouse_btn.is_none() && mode.contains(TermMode::MOUSE_MOTION) && area.contains(*pos);
                            if !(held || free) || cell == self.mouse_cell { continue; }
                            (self.mouse_btn.unwrap_or(3) + 32, cell, true)
                        }
                        _ => continue,
                    };
                    self.mouse_cell = cell;
                    if let Some(bytes) = mouse_report(code, cell.0, cell.1, press, sgr) {
                        let _ = self.tx.send(Msg::Input(bytes.into()));
                    }
                }
            });
        }

        // Wheel: the pane under the pointer reports it to the program, turns it into arrow keys on the
        // alternate screen (less, man...), or scrolls the history.
        let lines = ui.input(|i| (i.smooth_scroll_delta.y / ch).round() as i32);
        if lines != 0 && resp.hovered() {
            let n = lines.unsigned_abs().min(5);
            if mouse {
                let (c, r) = resp.hover_pos().map(cell_at).unwrap_or((0, 0));
                for _ in 0..n {
                    if let Some(b) = mouse_report(if lines > 0 { 64 } else { 65 }, c, r, true, sgr) {
                        let _ = self.tx.send(Msg::Input(b.into()));
                    }
                }
            } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
                let key = if lines > 0 { Key::ArrowUp } else { Key::ArrowDown };
                for _ in 0..n {
                    if let Some(b) = key_bytes(key, false, false, false, app_cursor) { let _ = self.tx.send(Msg::Input(b.into())); }
                }
            } else {
                term.scroll_display(Scroll::Delta(lines));
            }
        }
        // Dead keys (´ + a = á) only compose when the window has IME enabled; egui does that while an IME rect is set.
        if focused && !v.block {
            ui.ctx().output_mut(|o| o.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Terminal,
                rect,
                cursor_rect: Rect::from_min_size(rect.min, Vec2::new(cw, ch)),
                should_interrupt_composition: false,
            }));
        }
        if focused && !v.block { ui.input(|i| {
            let typed = std::cell::Cell::new(false);
            let send = |b: Vec<u8>| {
                typed.set(true);
                let _ = self.tx.send(Msg::Input(b.into()));
            };
            for e in &i.events {
                match e {
                    // Option as Meta: the key press below sends ESC+letter, so drop the composed character (å, ƒ...).
                    egui::Event::Text(_) if meta && i.modifiers.alt => {}
                    egui::Event::Text(t) => send(t.clone().into_bytes()),
                    egui::Event::Ime(egui::ImeEvent::Commit(t)) => send(t.clone().into_bytes()),
                    egui::Event::Paste(t) => send(paste_bytes(t, bracketed)),
                    egui::Event::Copy => copy = true,
                    // Off macOS egui turns Ctrl+C / Ctrl+X into Copy/Cut events instead of key presses; the shell still needs them.
                    egui::Event::Cut if !cfg!(target_os = "macos") => send(vec![0x18]),
                    egui::Event::Key { key, pressed: true, modifiers: m, .. } => {
                        // macOS editing shortcuts: Cmd = line, Alt = word.
                        let cmd = m.command && cfg!(target_os = "macos");
                        let b: Option<&[u8]> = match (key, cmd, m.alt) {
                            (Key::Backspace, true, _) => Some(b"\x15"), // kill line (Ctrl+U)
                            (Key::ArrowLeft, true, false) => Some(b"\x01"),
                            (Key::ArrowRight, true, false) => Some(b"\x05"),
                            (Key::Backspace, _, true) => Some(b"\x17"), // kill word (Ctrl+W)
                            (Key::ArrowLeft, _, true) => Some(b"\x1bb"),
                            (Key::ArrowRight, _, true) => Some(b"\x1bf"),
                            _ => None,
                        };
                        let name = key.name();
                        let meta_char = (meta && m.alt && !cmd && name.len() == 1 && name.is_ascii())
                            .then(|| if m.shift { name.to_ascii_uppercase() } else { name.to_ascii_lowercase() });
                        if let Some(b) = b {
                            send(b.to_vec());
                        } else if let Some(c) = meta_char {
                            send(format!("\x1b{c}").into_bytes());
                        } else if !is_app_shortcut(m)
                            && let Some(b) = key_bytes(*key, m.shift, m.alt, m.ctrl, app_cursor) { send(b) }
                    }
                    _ => {}
                }
            }
            // Like any terminal: typing (or Ctrl+C...) jumps back to the prompt if you were scrolled up in the history.
            if typed.get() { term.scroll_display(Scroll::Bottom); }
        }); }
        if copy {
            match term.selection_to_string() {
                Some(s) => ui.ctx().copy_text(s),
                // Off macOS Ctrl+C is both "copy" and "interrupt": with nothing selected it is the interrupt.
                None if !cfg!(target_os = "macos") => {
                    let _ = self.tx.send(Msg::Input(vec![3].into()));
                    term.scroll_display(Scroll::Bottom);
                }
                None => {}
            }
        }

        // Right-click menu (when the program is not capturing the mouse). Paste arrives as Event::Paste next frame.
        if !mouse {
            resp.context_menu(|ui| {
                let sel = term.selection_to_string();
                if ui.add_enabled(sel.is_some(), egui::Button::new("Copy")).clicked() {
                    if let Some(s) = sel { ui.ctx().copy_text(s); }
                    ui.close();
                }
                if ui.button("Paste").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                    ui.close();
                }
                if ui.button("Copy last output").clicked() {
                    if let Some(s) = last_block(&term) { ui.ctx().copy_text(s); }
                    ui.close();
                }
            });
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
        let p = ui.painter_at(area);
        p.rect_filled(area, 0.0, cfg().bg);
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
            let bold_face = f.contains(Flags::BOLD) && BOLD_OK.load(Ordering::Relaxed);
            if f.contains(Flags::BOLD) && !bold_face { fg = fg.lerp_to_gamma(Color32::WHITE, 0.25); } // no bold face: brighten
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
                font_id: if bold_face { FontId::new(font.size, egui::FontFamily::Name("bold".into())) } else { font.clone() },
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
            // Zero-width marks (emoji variation selector U+FE0F, ZWJ, skin tones, combining accents) live beside the base char.
            let mut cs = String::from(c);
            if let Some(z) = cell.zerowidth() { cs.extend(z); }
            match job.sections.last_mut() {
                Some(sec) if sec.format == fmt => {
                    job.text.push_str(&cs);
                    sec.byte_range.end = egui::text::ByteIndex(job.text.len());
                }
                _ => job.append(&cs, 0.0, fmt),
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
            let c = cfg().accent;
            for r in [
                Rect::from_min_size(area.min, Vec2::new(area.width(), 1.0)),
                Rect::from_min_size(Pos2::new(area.left(), area.bottom() - 1.0), Vec2::new(area.width(), 1.0)),
                Rect::from_min_size(area.min, Vec2::new(1.0, area.height())),
                Rect::from_min_size(Pos2::new(area.right() - 1.0, area.top()), Vec2::new(1.0, area.height())),
            ] {
                p.rect_filled(r, 0.0, c);
            }
        }
        want_focus
    }
}

pub struct Row {
    pub line: i32,
    pub next: usize,
    pub items: Vec<(usize, egui::text::LayoutJob)>,
    pub bgs: Vec<(usize, usize, Color32)>,
}

/// The http(s) URL under `pt`, if any (whitespace-delimited token on the visible row).
pub fn url_at(term: &Term<Proxy>, pt: Point) -> Option<String> {
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
pub fn detach(mut cmd: std::process::Command) {
    std::thread::spawn(move || { let _ = cmd.status(); });
}

pub fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    let mut c = std::process::Command::new(opener);
    c.arg(url);
    detach(c);
}

/// Desktop notification. The text goes in as an argument, never into a script, since it can come from the terminal.
pub fn notify(body: &str) {
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
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let body: String = text
        .replace("\r\n", "\r")
        .replace('\n', "\r")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\r' | '\t'))
        .collect();
    if bracketed { format!("\x1b[200~{body}\x1b[201~").into_bytes() } else { body.into_bytes() }
}

/// Is this key chord one of the app's own shortcuts (tabs, splits, search...) rather than terminal input?
/// Cmd on macOS; Ctrl+Shift elsewhere, so plain Ctrl+letter stays available to the shell.
pub fn is_app_shortcut(m: &egui::Modifiers) -> bool {
    if cfg!(target_os = "macos") { m.command } else { m.ctrl && m.shift }
}

/// xterm mouse report. `code`: 0/1/2 = left/middle/right (+4 shift, +8 alt, +16 ctrl, +32 motion),
/// 64/65 = wheel up/down. `col`/`row` are 0-based cells. None if the legacy encoding can't hold the position.
pub fn mouse_report(code: u8, col: usize, row: usize, press: bool, sgr: bool) -> Option<Vec<u8>> {
    let (x, y) = (col + 1, row + 1);
    if sgr {
        return Some(format!("\x1b[<{code};{x};{y}{}", if press { 'M' } else { 'm' }).into_bytes());
    }
    if x > 223 || y > 223 { return None; }
    let cb = if press { code } else { (code & !3) | 3 }; // legacy release has no button number
    Some(vec![0x1b, b'[', b'M', 32 + cb, 32 + x as u8, 32 + y as u8])
}

pub fn rgb(c: Color, default: Color32) -> Color32 {
    match c {
        Color::Spec(c) => Color32::from_rgb(c.r, c.g, c.b),
        Color::Named(n) if (n as usize) < 16 => cfg().ansi[n as usize],
        Color::Named(NamedColor::Background) => cfg().bg,
        Color::Named(_) => default,
        Color::Indexed(i) if i < 16 => cfg().ansi[i as usize],
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

/// Bytes a key sends to the program (xterm style). Modifiers follow xterm's `1 + shift + 2*alt + 4*ctrl`.
pub fn key_bytes(key: Key, shift: bool, alt: bool, ctrl: bool, app_cursor: bool) -> Option<Vec<u8>> {
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let plain = m == 1;
    // Arrows, Home, End: ESC [ A (ESC O A in application mode), ESC [ 1 ; m A with modifiers.
    let letter = |c: char| {
        if !plain { format!("\x1b[1;{m}{c}") } else if app_cursor { format!("\x1bO{c}") } else { format!("\x1b[{c}") }.into_bytes()
    };
    // F1-F4: ESC O P..S, ESC [ 1 ; m P..S with modifiers.
    let ss3 = |c: char| if plain { format!("\x1bO{c}") } else { format!("\x1b[1;{m}{c}") }.into_bytes();
    // PageUp, Delete, F5...: ESC [ n ~, ESC [ n ; m ~ with modifiers.
    let tilde = |n: u8| if plain { format!("\x1b[{n}~") } else { format!("\x1b[{n};{m}~") }.into_bytes();
    Some(match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => vec![0x7f],
        Key::Tab if shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => vec![0x1b],
        Key::ArrowUp => letter('A'),
        Key::ArrowDown => letter('B'),
        Key::ArrowRight => letter('C'),
        Key::ArrowLeft => letter('D'),
        Key::Home => letter('H'),
        Key::End => letter('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => ss3('P'),
        Key::F2 => ss3('Q'),
        Key::F3 => ss3('R'),
        Key::F4 => ss3('S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        Key::Space if ctrl => vec![0],
        k if ctrl => {
            let name = k.name();
            let c = name.chars().next().filter(|_| name.len() == 1)?.to_ascii_lowercase();
            if !c.is_ascii_lowercase() { return None; }
            vec![c as u8 - b'a' + 1]
        }
        _ => return None,
    })
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
mod block_tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Processor;
    #[derive(Clone)] struct L;
    impl EventListener for L {}
    #[test]
    fn copies_only_last_command() {
        let mut t = Term::new(Config::default(), &Size { cols: 20, rows: 10 }, L);
        let mut p: Processor = Processor::new();
        p.advance(&mut t, b"$ one\r\n\x1b]8;id=1;own:block\x07A\r\nB\r\n\r\nC\r\n\x1b]8;;\x07$ two\r\n\x1b]8;id=2;own:block\x07X\r\nY\r\n\x1b]8;;\x07$ ");
        assert_eq!(last_block(&t).unwrap(), "X\nY");
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

#[cfg(test)]
mod mouse_tests {
    use super::mouse_report;

    #[test]
    fn reports_in_both_encodings() {
        assert_eq!(mouse_report(0, 0, 0, true, true).unwrap(), b"\x1b[<0;1;1M");
        assert_eq!(mouse_report(0, 9, 4, false, true).unwrap(), b"\x1b[<0;10;5m");
        assert_eq!(mouse_report(64, 2, 2, true, true).unwrap(), b"\x1b[<64;3;3M");
        assert_eq!(mouse_report(0, 0, 0, true, false).unwrap(), [0x1b, b'[', b'M', 32, 33, 33]);
        assert_eq!(mouse_report(2, 0, 0, false, false).unwrap()[3], 32 + 3); // legacy release
        assert!(mouse_report(0, 300, 0, true, false).is_none());
    }
}

#[cfg(test)]
mod key_tests {
    use super::key_bytes;
    use eframe::egui::Key;

    #[test]
    fn xterm_sequences() {
        let k = |key, shift, alt, ctrl, app| key_bytes(key, shift, alt, ctrl, app).unwrap();
        assert_eq!(k(Key::ArrowUp, false, false, false, false), b"\x1b[A");
        assert_eq!(k(Key::ArrowUp, false, false, false, true), b"\x1bOA"); // application cursor keys
        assert_eq!(k(Key::ArrowRight, false, false, true, false), b"\x1b[1;5C"); // Ctrl+Right
        assert_eq!(k(Key::ArrowLeft, true, false, false, true), b"\x1b[1;2D"); // Shift+Left ignores app mode
        assert_eq!(k(Key::F1, false, false, false, false), b"\x1bOP");
        assert_eq!(k(Key::F5, false, false, false, false), b"\x1b[15~");
        assert_eq!(k(Key::F12, true, false, false, false), b"\x1b[24;2~");
        assert_eq!(k(Key::Tab, true, false, false, false), b"\x1b[Z");
        assert_eq!(k(Key::Delete, false, false, true, false), b"\x1b[3;5~");
        assert_eq!(k(Key::Space, false, false, true, false), [0]);
        assert_eq!(k(Key::C, false, false, true, false), [3]);
        assert!(key_bytes(Key::A, false, false, false, false).is_none()); // plain letters arrive as Text
    }
}

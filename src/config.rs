use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Arc;


use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle};
use eframe::egui::{self, Color32};

// User settings from ~/.config/own-terminal/config: `key = value` lines, `#` comments.
// Keys: font_size, font_path / font_bold_path (.ttf/.otf), theme, fg, bg and color0..color15 (#rrggbb),
// cursor (block|bar|underline), cursor_blink (true|false),
// padding (px around the text), accent (#rrggbb: active tab and focused pane), scrollback (lines of history),
// option_as_meta (true: Option/Alt+letter sends ESC+letter, for vim/emacs/readline),
// notify_after (seconds a command must run before a notification when the window is in background; 0 = off).
pub struct Settings {
    pub font_size: f32,
    pub font_path: Option<String>,
    pub font_bold_path: Option<String>,
    pub fg: Color32,
    pub bg: Color32,
    pub bar: Color32,
    pub ansi: [Color32; 16],
    pub cursor: CursorStyle,
    pub notify_after: u64,
    pub padding: f32,
    pub accent: Color32,
    pub scrollback: usize,
    pub option_as_meta: bool,
}

/// (name, fg, bg, the 16 ANSI colors). The first entry is the default.
pub const THEMES: [(&str, u32, u32, [u32; 16]); 5] = [
    ("default", 0xd8d8d8, 0x181818, [
        0x181818, 0xab4642, 0xa1b56c, 0xf7ca88, 0x7cafc2, 0xba8baf, 0x86c1b9, 0xd8d8d8,
        0x585858, 0xdc5c5c, 0xb5d37f, 0xffd98d, 0x8ec6dc, 0xd29ec8, 0x9bd6cd, 0xf8f8f8,
    ]),
    ("dracula", 0xf8f8f2, 0x282a36, [
        0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
        0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
    ]),
    ("solarized-dark", 0x839496, 0x002b36, [
        0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
        0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
    ]),
    ("gruvbox-dark", 0xebdbb2, 0x282828, [
        0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
        0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
    ]),
    ("nord", 0xd8dee9, 0x2e3440, [
        0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
        0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
    ]),
];

pub fn hex(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

/// Drops a trailing `# comment` (a `#` after whitespace), so `bg = #181818  # dark` works.
fn strip_comment(v: &str) -> &str {
    let v = v.trim();
    match v.char_indices().find(|&(i, ch)| ch == '#' && i > 0 && v[..i].ends_with(char::is_whitespace)) {
        Some((i, _)) => &v[..i],
        None => v,
    }
}

pub fn parse(text: &str) -> Settings {
    let (_, fg, bg, ansi) = THEMES[0];
    let mut s = Settings {
        font_size: 14.0,
        font_path: None,
        font_bold_path: None,
        fg: hex(fg),
        bg: hex(bg),
        bar: Color32::BLACK,
        ansi: ansi.map(hex),
        cursor: CursorStyle { shape: CursorShape::Block, blinking: false },
        notify_after: 10,
        padding: 6.0,
        accent: Color32::TRANSPARENT,
        scrollback: 10_000,
        option_as_meta: false,
    };
    let mut accent = None;
    let color = |v: &str| u32::from_str_radix(v.trim_start_matches('#'), 16).ok().map(hex);
    let lines: Vec<(&str, &str)> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), strip_comment(v).trim()))
        .collect();
    // The theme goes first so fg/bg/colorN lines override it wherever they appear.
    if let Some((_, name)) = lines.iter().find(|(k, _)| *k == "theme") {
        if let Some((_, fg, bg, ansi)) = THEMES.iter().find(|t| t.0 == *name) {
            (s.fg, s.bg, s.ansi) = (hex(*fg), hex(*bg), ansi.map(hex));
        }
    }
    for (k, v) in lines {
        match k {
            "font_size" => s.font_size = v.parse().unwrap_or(s.font_size),
            "font_path" => s.font_path = Some(v.to_string()),
            "font_bold_path" => s.font_bold_path = Some(v.to_string()),
            "fg" => s.fg = color(v).unwrap_or(s.fg),
            "bg" => s.bg = color(v).unwrap_or(s.bg),
            "cursor" => match v {
                "block" => s.cursor.shape = CursorShape::Block,
                "bar" => s.cursor.shape = CursorShape::Beam,
                "underline" => s.cursor.shape = CursorShape::Underline,
                _ => {}
            },
            "cursor_blink" => s.cursor.blinking = v == "true",
            "notify_after" => s.notify_after = v.parse().unwrap_or(s.notify_after),
            "padding" => s.padding = v.parse().map(|p: f32| p.clamp(0.0, 40.0)).unwrap_or(s.padding),
            "scrollback" => s.scrollback = v.parse().map(|n: usize| n.min(1_000_000)).unwrap_or(s.scrollback),
            "accent" => accent = color(v),
            "option_as_meta" => s.option_as_meta = v == "true",
            _ => {
                if let Some(n) = k.strip_prefix("color").and_then(|n| n.parse::<usize>().ok()).filter(|n| *n < 16) {
                    s.ansi[n] = color(v).unwrap_or(s.ansi[n]);
                }
            }
        }
    }
    s.bar = s.bg.gamma_multiply(0.6);
    s.accent = accent.unwrap_or(s.fg.gamma_multiply(0.45));
    s
}

/// $HOME, or %USERPROFILE% on Windows.
pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(Into::into)
}

pub fn load() -> Settings {
    let path = home_dir().map(|h| h.join(".config/own-terminal/config"));
    parse(&path.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default())
}

// Settings are leaked on (re)load so cfg() can hand out plain references; a reload is rare and tiny.
pub static CFG: AtomicPtr<Settings> = AtomicPtr::new(std::ptr::null_mut());

pub fn cfg() -> &'static Settings {
    let mut p = CFG.load(Ordering::Acquire);
    if p.is_null() {
        reload();
        p = CFG.load(Ordering::Acquire);
    }
    unsafe { &*p }
}

pub fn reload() {
    CFG.store(Box::leak(Box::new(load())), Ordering::Release);
}

/// True when a bold face was loaded (otherwise bold text is just drawn brighter).
pub static BOLD_OK: AtomicBool = AtomicBool::new(false);

pub fn load_font(ctx: &egui::Context) {
    let read = |p: &Option<String>| p.as_ref().and_then(|p| std::fs::read(p).ok());
    let mut fonts = egui::FontDefinitions::default();
    if let Some(bytes) = read(&cfg().font_path) {
        fonts.font_data.insert("custom".into(), Arc::new(egui::FontData::from_owned(bytes)));
        fonts.families.entry(egui::FontFamily::Monospace).or_default().insert(0, "custom".into());
    }
    let bold = read(&cfg().font_bold_path);
    BOLD_OK.store(bold.is_some(), Ordering::Relaxed);
    if let Some(bytes) = bold {
        fonts.font_data.insert("bold".into(), Arc::new(egui::FontData::from_owned(bytes)));
        fonts.families.insert(egui::FontFamily::Name("bold".into()), vec!["bold".into()]);
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn theme_then_overrides() {
        let s = parse("color1 = #ff0000\ntheme = nord\nbg = #000000\n# color2 = #123456\ncolor99 = #ffffff");
        assert_eq!(s.fg, hex(0xd8dee9)); // from the theme
        assert_eq!(s.bg, hex(0x000000)); // overridden
        assert_eq!(s.ansi[1], hex(0xff0000)); // override listed before the theme still wins
        assert_eq!(s.ansi[2], hex(0xa3be8c)); // commented line ignored
    }

    #[test]
    fn layout_options() {
        let s = parse("padding = 99\nscrollback = 500\naccent = #ff00ff");
        assert_eq!(s.padding, 40.0); // clamped
        assert_eq!(s.scrollback, 500);
        assert_eq!(s.accent, hex(0xff00ff));
        assert_eq!(parse("").scrollback, 10_000);
    }

    #[test]
    fn trailing_comments() {
        let s = parse("theme = dracula   # a comment\nbg = #101010 # dark\ncursor = bar\t# tab");
        assert_eq!(s.fg, hex(0xf8f8f2)); // theme name was not polluted by the comment
        assert_eq!(s.bg, hex(0x101010)); // a leading # (hex color) is kept
        assert_eq!(s.cursor.shape, CursorShape::Beam);
    }
}

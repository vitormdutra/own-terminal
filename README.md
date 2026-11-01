# own-terminal

[![CI](https://github.com/vitormdutra/own-terminal/actions/workflows/ci.yml/badge.svg)](https://github.com/vitormdutra/own-terminal/actions/workflows/ci.yml)

A clean, fast, lightweight terminal emulator written in Rust. No Electron, no web view, no
telemetry, no settings maze: one small native binary with tabs, splits, search and a single plain-text
config file.

Terminal emulation comes from [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal)
(the engine behind Alacritty); the window and rendering are [`egui`/`eframe`](https://github.com/emilk/egui).

> **Status:** early. Daily-driven on macOS. The code compiles for Linux (type-checked), but it has
> not been run there yet, and Windows has not been checked at all. Bug reports are welcome.

## Features

- **Tabs and splits**: side-by-side and stacked panes in the same tab, draggable dividers, maximize a
  pane, move focus with the keyboard, reorder tabs by dragging. New tabs and splits open in the
  current directory.
- **Search** the scrollback (`Cmd+F`), case-insensitive, with highlighted matches.
- **Selection** by drag, word (double click) or line (triple click). `Cmd+click` opens URLs.
- **Mouse support** inside full-screen programs (`vim`, `htop`, ...); hold `Shift` to select text
  anyway.
- **Full color**: 16 / 256 / truecolor, italic, underline, strikethrough, optional real bold font.
  Five built-in themes plus per-color overrides.
- **Proper keyboard handling**: xterm sequences for F-keys and modified arrows, bracketed paste,
  optional Option-as-Meta.
- **Long-command notifications**: get a desktop notification (and a Dock bounce) when a command that
  ran for 10+ seconds finishes while the window is in the background. No shell integration needed.
- **Zoom** the font at runtime, **reload** the config without restarting.
- Lightweight: roughly 0.2 ms of UI time per frame, 40-60 frames/s while a flood of colored output
  scrolls by (see [Performance](#performance)).

## Install

### Download a build

Grab the latest from the [Releases page](https://github.com/vitormdutra/own-terminal/releases):

- **macOS (Apple Silicon):** `OwnTerminal-macos-apple-silicon.zip`. Unzip it, drag `OwnTerminal.app`
  to `/Applications`, and open it. The app is not notarized by Apple, so the first launch needs
  right-click → **Open** → **Open**. If macOS says the app is "damaged", it is only the download
  quarantine flag; clear it with
  `xattr -dr com.apple.quarantine /Applications/OwnTerminal.app`.
- **Linux (x86_64):** `own-terminal-linux-x86_64.tar.gz` contains the binary; put it somewhere on
  your `PATH`. (Not yet tested on a real desktop, see [Linux and Windows](#linux-and-windows).)

Intel Macs and Windows have no prebuilt binary yet; build from source (below).

### Build from source

You need a [Rust toolchain](https://rustup.rs) (stable).

```bash
git clone https://github.com/vitormdutra/own-terminal.git
cd own-terminal
cargo install --path .
```

This installs the `own-terminal` binary into `~/.cargo/bin`. To try it without installing:
`cargo run --release`.

### macOS app (Spotlight / Dock)

```bash
./bundle.sh
cp -R dist/OwnTerminal.app /Applications/
```

The app is not code-signed, so the first time macOS may refuse to open it: right-click
`OwnTerminal` → **Open** → **Open**. After that it is a normal app you can launch from Spotlight or
pin to the Dock. It starts in your home directory.

### Linux / Windows

`cargo install --path .` works the same way; there is no installer or `.desktop` file yet.
See [Linux and Windows](#linux-and-windows) for the differences.

### Update / uninstall

```bash
git pull && cargo install --path . && ./bundle.sh   # ./bundle.sh is macOS only
cargo uninstall own-terminal
```

## Usage

Run `own-terminal` (or open the app). It starts your login shell (`$SHELL`).

On macOS the app shortcuts use `Cmd`. On Linux and Windows, replace `Cmd` with `Ctrl+Shift` (so plain
`Ctrl+letter` always reaches your shell); the few exceptions are noted in the table.

| Action | Shortcut |
|--------|----------|
| New tab | `Cmd+T`, or the `+` button |
| Close pane / tab / window | `Cmd+W` closes the focused pane; the tab closes with its last pane, the window with its last tab. Tabs also have a `×` button |
| Go to tab N | `Cmd+1` … `Cmd+9`, or click the tab |
| Reorder tabs | Drag a tab |
| Split side by side | `Cmd+D` |
| Split stacked | `Cmd+Shift+D` (Linux/Windows: `Ctrl+Shift+Alt+D`). You can mix both directions |
| Resize a split | Drag the divider |
| Move focus between panes | `Cmd+Option+←↑↓→` (the neighbour in that direction), or `Cmd+]` / `Cmd+[` (next / previous), or click |
| Maximize / restore pane | `Cmd+Shift+Enter` |
| Search scrollback | `Cmd+F`; `Enter` = previous (older) match, `Shift+Enter` = next, `Esc` closes |
| Select text | Drag; double click = word; triple click = line; click to clear |
| Copy | `Cmd+C` (Linux/Windows: `Ctrl+C` *with a selection*; with no selection it interrupts the program as usual) |
| Paste | `Cmd+V` (Linux/Windows: `Ctrl+V` or `Ctrl+Shift+V`) |
| Open a link | `Cmd+click` on an `http(s)://` URL (Linux/Windows: `Ctrl+click`) |
| Scroll history | Mouse wheel / trackpad |
| Font zoom | `Cmd+=` bigger, `Cmd+-` smaller, `Cmd+0` back to the configured size |
| Reload config | `Cmd+,` |
| Delete line / word | `Cmd+Backspace` / `Option+Backspace` (macOS) |
| Start / end of line | `Cmd+←` / `Cmd+→` (macOS) |
| Previous / next word | `Option+←` / `Option+→` |

Notes:

- **Scrolling in full-screen programs.** In `less`, `man` and similar, the wheel sends arrow keys. In
  programs that ask for the mouse (`vim`, `htop`) it is sent to the program.
- **Search only sees the terminal's own history.** Full-screen programs such as `nano` or `vim` keep
  no history in the terminal, so use their own search (`Ctrl+W` in nano, `/` in vim and less).
- **Window title** follows the title set by the program in the focused pane.

## Configuration

Everything is optional. Create `~/.config/own-terminal/config` (on Windows, under `%USERPROFILE%`).
It is a plain `key = value` file; lines starting with `#` are comments, and so is anything after
` #` at the end of a line.

```ini
font_size = 15
font_path = /path/to/YourMono-Regular.ttf
font_bold_path = /path/to/YourMono-Bold.ttf   # optional, same family as font_path

theme = dracula           # default | dracula | solarized-dark | gruvbox-dark | nord
fg = #d8d8d8              # override the theme's foreground...
bg = #181818              # ...and background
color1 = #ff5555          # color0 .. color15: the 16 ANSI colors (0-7 normal, 8-15 bright)
accent = #7cafc2          # active tab marker and focused-pane frame

cursor = block            # block | bar | underline
cursor_blink = false
padding = 6               # pixels around the text
scrollback = 10000        # lines of history per pane

option_as_meta = false    # macOS: Option+letter sends ESC+letter (vim/emacs/readline)
notify_after = 10         # seconds; notify when a longer command finishes in the background (0 = off)
```

`theme` picks a preset; `fg`, `bg` and `colorN` override parts of it regardless of their order in the
file. After editing, press `Cmd+,` to reload. Colors, fonts and size apply immediately; cursor shape
applies to new panes.

**Fonts.** The built-in font has no bold face, so bold text is drawn in a brighter color. For real bold,
set `font_bold_path` to the bold file of the *same* family as `font_path` (the glyph widths must match
or the grid misaligns). Use `.ttf` / `.otf` files; `.ttc` collections are not supported.

**Option as Meta.** By default `Option+letter` types the usual macOS symbols and accents. Turning
`option_as_meta` on makes it send `ESC` + letter instead, which is what vim, emacs and readline expect.

**Notifications.** The terminal tracks which process owns the foreground of the pty, so it needs no
shell setup. It assumes a pane is idle two seconds after it opens. On macOS the notification appears
under the name "Script Editor"; if you do not see it, allow it in System Settings → Notifications.

## Linux and Windows

The code avoids platform-specific APIs apart from `bundle.sh` and desktop notifications, and
`cargo check --target x86_64-unknown-linux-gnu` passes. It has **not been run** outside macOS, so
expect rough edges. Differences you should know about:

- App shortcuts are `Ctrl+Shift+…` instead of `Cmd+…`; stacked split is `Ctrl+Shift+Alt+D`.
- `Ctrl+C` copies only when text is selected, otherwise it sends the interrupt.
- Notifications use `notify-send` on Linux; they are not implemented on Windows.
- Directory inheritance for new tabs uses `/proc` on Linux; on Windows new tabs open in the default
  directory.

## Performance

Start it with `OWN_TERMINAL_PROFILE=1 own-terminal` and, once per second, it prints to stderr the frames
per second and the average and worst time spent building the UI. With about 800,000 lines of colored
text streamed in a row, I measured 40-60 frames/s at roughly 0.2 ms average per frame (3 ms worst case).
GPU time is not included in that number.

## Known limitations

- Search matches plain text only (no regex) and does not refresh on its own as new output arrives;
  retype to search again.
- Bold needs `font_bold_path`; otherwise it is only a brighter color.
- No ligatures, no image protocols, no IME-specific handling beyond what `egui` provides.
- Panes cannot be swapped or dragged to another split.
- The macOS app is only ad-hoc signed, not notarized, so Gatekeeper asks for confirmation on first launch.

## Development

```text
src/main.rs    app, tab bar, shortcuts, search bar
src/pane.rs    one terminal: pty, input, mouse, drawing, search
src/layout.rs  split tree, dividers, directional focus
src/config.rs  config parsing, themes, fonts
```

```bash
cargo test --release     # unit tests; one of them starts a real pty and takes ~8 s
cargo clippy             # if you have it installed
```

The foreground-process test spawns `/bin/sh` on a pty, so it needs a Unix system. Every push and pull
request builds and tests on macOS and Linux through GitHub Actions (`.github/workflows/ci.yml`).

### Publishing a release

Bump `version` in `Cargo.toml`, commit, then push a tag:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

`.github/workflows/release.yml` builds the macOS app and the Linux binary and attaches them to a new
GitHub release with generated notes.

## Acknowledgements

Built on [alacritty_terminal](https://github.com/alacritty/alacritty) and
[egui](https://github.com/emilk/egui).

## License

[MIT](LICENSE)

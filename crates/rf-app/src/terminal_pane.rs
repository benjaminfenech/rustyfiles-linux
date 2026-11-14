//! A terminal inside rfiles, under the file list ("Run command here", F4): your shell in a
//! PTY, drawn with GPUI on top of alacritty_terminal's emulator (escape codes, colours,
//! scrollback, full-screen programs).
//!
//! It follows the folder you browse to while nothing runs in it (" cd …" is typed for you),
//! and a `cd` typed in it moves the file list along ([`TerminalEvent::CwdChanged`]).

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use futures::StreamExt as _;
use gpui_kit::*;

use crate::theme::Palette;

const FONT_SIZE: f32 = 13.;

#[derive(Clone)]
struct Listener(futures::channel::mpsc::UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, e: TermEvent) {
        let _ = self.0.unbounded_send(e);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub enum TerminalEvent {
    /// The shell's working folder changed (a `cd` typed in it).
    CwdChanged(PathBuf),
    /// The shell ended (`exit`, Ctrl+D).
    Exited,
}

pub struct TerminalPane {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    /// The shell's process (its own process group: it's in the foreground when idle).
    shell_pid: u32,
    /// The PTY's master side, to ask who's in the foreground.
    master: File,
    focus: FocusHandle,
    size: GridSize,
    /// Cell width and line height, measured from the font.
    cell: Size<Pixels>,
    /// Where the grid was drawn (mouse positions → cells).
    origin: Point2,
    title: String,
    /// The shell's folder as last seen in /proc.
    cwd: Option<PathBuf>,
    /// A " cd" typed for the file list that hasn't happened yet.
    pending_cd: Option<PathBuf>,
    selecting: bool,
    exited: bool,
    /// Inside Flatpak the shell runs on the host: its folder can't be followed.
    on_host: bool,
}

/// A pixel position (GPUI's `Point` clashes with the terminal's).
type Point2 = gpui_kit::Point<Pixels>;

impl EventEmitter<TerminalEvent> for TerminalPane {}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

thread_local! {
    /// Every terminal pane, to find the focused one for a keystroke ([`intercept`]).
    static PANES: std::cell::RefCell<Vec<WeakEntity<TerminalPane>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Sends keystrokes to the focused terminal pane before the app's shortcuts see them, so
/// Ctrl+W deletes a word there instead of closing the tab. F4 still hides the pane. (A key
/// stopped here doesn't reach the pane's own key listeners either, so it's handled here.)
pub fn intercept(cx: &mut App) {
    cx.intercept_keystrokes(|ev, window, cx| {
        if ev.keystroke.key == "f4" || !ev.context_stack.iter().any(|c| c.contains("TerminalPane")) {
            return;
        }
        let panes: Vec<_> = PANES.with(|p| {
            let mut p = p.borrow_mut();
            p.retain(|w| w.upgrade().is_some());
            p.clone()
        });
        let focused = panes.into_iter().filter_map(|w| w.upgrade()).find(|t| t.read(cx).focus.is_focused(window));
        if let Some(pane) = focused {
            let ks = ev.keystroke.clone();
            pane.update(cx, |t, cx| t.on_key(&ks, cx));
            cx.stop_propagation();
        }
    })
    .detach();
}

/// The desktop's monospace font (fontconfig's `monospace`), looked up once.
fn mono_family() -> SharedString {
    static FAMILY: OnceLock<SharedString> = OnceLock::new();
    FAMILY
        .get_or_init(|| {
            std::process::Command::new("fc-match")
                .args(["-f", "%{family[0]}", "monospace"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|f| !f.is_empty())
                .unwrap_or_else(|| "DejaVu Sans Mono".to_string())
                .into()
        })
        .clone()
}

/// A shell started in a PTY, before its pane exists (so a failure can be shown instead).
pub struct Started {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    shell_pid: u32,
    master: File,
    rx: futures::channel::mpsc::UnboundedReceiver<TermEvent>,
    size: GridSize,
    dir: PathBuf,
    on_host: bool,
}

impl TerminalPane {
    /// Starts your shell in `dir`, `cols` × `rows` cells (the pane's size as far as it's
    /// known before it's drawn: output written at another width gets rewrapped).
    pub fn start(dir: &Path, cols: usize, rows: usize) -> anyhow::Result<Started> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let listener = Listener(tx);
        let size = GridSize { cols: cols.max(2), rows: rows.max(1) };
        let config = Config { scrolling_history: 10_000, ..Config::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));
        // TERM_PROGRAM tells shell startup files where they run (`[[ $TERM_PROGRAM == rfiles ]]`)
        let env: HashMap<String, String> = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "rfiles")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let on_host = rf_linux::sandbox::flatpak_id().is_some();
        // inside Flatpak: the user's shell on the host, not the sandbox's sh
        let shell = on_host.then(|| {
            tty::Shell::new(
                "flatpak-spawn".into(),
                vec![
                    "--host".into(),
                    "--watch-bus".into(),
                    format!("--directory={}", dir.display()),
                    "--env=TERM=xterm-256color".into(),
                    "--env=COLORTERM=truecolor".into(),
                    "--env=TERM_PROGRAM=rfiles".into(),
                    "sh".into(),
                    "-c".into(),
                    "exec \"${SHELL:-/bin/sh}\"".into(),
                ],
            )
        });
        let opts = tty::Options { shell, working_directory: Some(dir.to_path_buf()), drain_on_exit: false, env };
        let ws = WindowSize { num_lines: size.rows as u16, num_cols: size.cols as u16, cell_width: 8, cell_height: 16 };
        let pty = tty::new(&opts, ws, 0)?;
        let shell_pid = pty.child().id();
        let master = pty.file().try_clone()?;
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok(Started { term, sender, shell_pid, master, rx, size, dir: dir.to_path_buf(), on_host })
    }

    pub fn new(s: Started, cx: &mut Context<Self>) -> Self {
        let Started { term, sender, shell_pid, master, mut rx, size, dir, on_host } = s;
        let me = cx.entity().downgrade();
        PANES.with(|p| p.borrow_mut().push(me));
        cx.spawn(async move |this, cx| {
            while let Some(e) = rx.next().await {
                if this.update(cx, |t, cx| t.on_event(e, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            term,
            sender,
            shell_pid,
            master,
            focus: cx.focus_handle(),
            size,
            cell: gpui_kit::size(px(8.), px(16.)),
            origin: Point2::default(),
            title: String::new(),
            cwd: Some(dir),
            pending_cd: None,
            selecting: false,
            exited: false,
            on_host,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.sender.send(Msg::Input(bytes.into()));
    }

    /// Types `command` and runs it.
    pub fn run(&mut self, command: &str) {
        self.write(format!("{command}\r").into_bytes());
    }

    /// Nothing runs in the shell: it's at its prompt.
    fn idle(&self) -> bool {
        // SAFETY: a valid descriptor of the PTY's master side
        let fg = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        fg > 0 && fg as u32 == self.shell_pid
    }

    /// Moves the shell to `dir` (the file list went there), when it's idle and not there
    /// already. The line typed so far is cleared first (Ctrl+E, Ctrl+U), as Dolphin does.
    pub fn follow(&mut self, dir: &Path) {
        if self.exited || self.on_host || !dir.is_dir() {
            return;
        }
        if self.cwd.as_deref() == Some(dir) || self.pending_cd.as_deref() == Some(dir) || !self.idle() {
            return;
        }
        let quoted = format!("'{}'", dir.to_string_lossy().replace('\'', r"'\''"));
        self.write(format!("\x05\x15 cd -- {quoted}\r").into_bytes());
        self.pending_cd = Some(dir.to_path_buf());
    }

    fn on_event(&mut self, e: TermEvent, cx: &mut Context<Self>) {
        match e {
            TermEvent::Wakeup => {
                self.check_cwd(cx);
                cx.notify();
            }
            TermEvent::PtyWrite(s) => self.write(s.into_bytes()),
            TermEvent::Title(t) => {
                self.title = t;
                cx.notify();
            }
            TermEvent::ResetTitle => self.title.clear(),
            TermEvent::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            TermEvent::ClipboardLoad(_, format) => {
                let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
                self.write(format(&text).into_bytes());
            }
            TermEvent::ColorRequest(ix, format) => {
                let rgb = self.term.lock().colors()[ix].unwrap_or_else(|| default_rgb(ix, true));
                self.write(format(rgb).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => {
                let ws = self.window_size();
                self.write(format(ws).into_bytes());
            }
            TermEvent::ChildExit(_) | TermEvent::Exit => {
                if !self.exited {
                    self.exited = true;
                    cx.emit(TerminalEvent::Exited);
                }
            }
            TermEvent::Bell | TermEvent::CursorBlinkingChange | TermEvent::MouseCursorDirty => {}
        }
    }

    /// Tells the file list when a `cd` typed in the shell changed its folder.
    fn check_cwd(&mut self, cx: &mut Context<Self>) {
        if self.on_host {
            return;
        }
        let Ok(now) = std::fs::read_link(format!("/proc/{}/cwd", self.shell_pid)) else { return };
        if self.cwd.as_deref() == Some(now.as_path()) {
            return;
        }
        self.cwd = Some(now.clone());
        match self.pending_cd.take() {
            // our own cd landed: the file list is there already
            Some(p) if p == now => {}
            // a cd of ours is still on its way: this is a step before it
            Some(p) => self.pending_cd = Some(p),
            None => cx.emit(TerminalEvent::CwdChanged(now)),
        }
    }

    fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.size.rows as u16,
            num_cols: self.size.cols as u16,
            cell_width: f32::from(self.cell.width) as u16,
            cell_height: f32::from(self.cell.height) as u16,
        }
    }

    /// The pane was laid out at `bounds`: the grid takes as many cells as fit.
    fn set_bounds(&mut self, bounds: Bounds<Pixels>, cell: Size<Pixels>) {
        self.origin = bounds.origin;
        let cols = (f32::from(bounds.size.width) / f32::from(cell.width)).floor().max(2.) as usize;
        let rows = (f32::from(bounds.size.height) / f32::from(cell.height)).floor().max(1.) as usize;
        let size = GridSize { cols, rows };
        if size != self.size || cell != self.cell {
            self.size = size;
            self.cell = cell;
            self.term.lock().resize(size);
            let _ = self.sender.send(Msg::Resize(self.window_size()));
        }
    }

    /// The cell under a window position (and which half of it).
    fn cell_at(&self, pos: Point2) -> (Point, Side) {
        let x = f32::from(pos.x - self.origin.x).max(0.);
        let y = f32::from(pos.y - self.origin.y).max(0.);
        let cw = f32::from(self.cell.width);
        let col = ((x / cw) as usize).min(self.size.cols.saturating_sub(1));
        let row = ((y / f32::from(self.cell.height)) as usize).min(self.size.rows.saturating_sub(1));
        let side = if x % cw < cw / 2. { Side::Left } else { Side::Right };
        let offset = self.term.lock().grid().display_offset() as i32;
        (Point::new(Line(row as i32 - offset), Column(col)), side)
    }

    fn copy(&self, cx: &mut App) {
        if let Some(text) = self.term.lock().selection_to_string().filter(|t| !t.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&self, cx: &mut App) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else { return };
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if bracketed {
            // a pasted end marker would end the paste early
            let text = text.replace("\x1b[201~", "");
            self.write(format!("\x1b[200~{text}\x1b[201~").into_bytes());
        } else {
            self.write(text.into_bytes());
        }
    }

    fn on_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        let m = &ks.modifiers;
        // Ctrl+Shift+C / V: copy and paste (Ctrl+C stays the shell's interrupt)
        if m.control && m.shift && !m.alt {
            match ks.key.as_str() {
                "c" => return self.copy(cx),
                "v" => return self.paste(cx),
                _ => {}
            }
        }
        // Shift+PageUp / PageDown: through the history
        if m.shift && !m.control && matches!(ks.key.as_str(), "pageup" | "pagedown") {
            let page = self.size.rows as i32;
            self.term.lock().scroll_display(Scroll::Delta(if ks.key == "pageup" { page } else { -page }));
            return cx.notify();
        }
        let app_cursor = self.term.lock().mode().contains(TermMode::APP_CURSOR);
        if let Some(bytes) = key_bytes(ks, app_cursor) {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
            drop(term);
            self.write(bytes);
            cx.notify();
        }
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, cx: &mut Context<Self>) {
        let lines = match ev.delta {
            ScrollDelta::Lines(l) => l.y * 3.,
            ScrollDelta::Pixels(p) => f32::from(p.y) / f32::from(self.cell.height),
        };
        let lines = lines.round() as i32;
        if lines == 0 {
            return;
        }
        let mode = *self.term.lock().mode();
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // a full-screen program (less, man): the wheel moves its cursor
            let key: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let mut out = Vec::new();
            for _ in 0..lines.unsigned_abs() {
                out.extend_from_slice(key);
            }
            self.write(out);
        } else {
            self.term.lock().scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }
}

/// What a key sends to the shell (xterm's sequences).
fn key_bytes(ks: &Keystroke, app_cursor: bool) -> Option<Cow<'static, [u8]>> {
    let m = &ks.modifiers;
    // xterm's modifier parameter: 1 + Shift + 2·Alt + 4·Ctrl
    let param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8;
    let csi = |end: char| -> Cow<'static, [u8]> {
        if param > 1 {
            format!("\x1b[1;{param}{end}").into_bytes().into()
        } else if app_cursor {
            format!("\x1bO{end}").into_bytes().into()
        } else {
            format!("\x1b[{end}").into_bytes().into()
        }
    };
    let tilde = |n: u8| -> Cow<'static, [u8]> {
        if param > 1 { format!("\x1b[{n};{param}~").into_bytes().into() } else { format!("\x1b[{n}~").into_bytes().into() }
    };
    let alt = |b: &[u8]| -> Cow<'static, [u8]> {
        let mut v = Vec::with_capacity(b.len() + 1);
        if m.alt {
            v.push(0x1b);
        }
        v.extend_from_slice(b);
        v.into()
    };
    let out = match ks.key.as_str() {
        "enter" => alt(b"\r"),
        "backspace" if m.control => alt(b"\x08"),
        "backspace" => alt(b"\x7f"),
        "tab" if m.shift => b"\x1b[Z".as_slice().into(),
        "tab" => alt(b"\t"),
        "escape" => alt(b"\x1b"),
        "up" => csi('A'),
        "down" => csi('B'),
        "right" => csi('C'),
        "left" => csi('D'),
        "home" => csi('H'),
        "end" => csi('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" | "f2" | "f3" | "f4" => {
            let end = ["P", "Q", "R", "S"][ks.key[1..].parse::<usize>().ok()? - 1];
            if param > 1 { format!("\x1b[1;{param}{end}").into_bytes().into() } else { format!("\x1bO{end}").into_bytes().into() }
        }
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "space" if m.control => b"\x00".as_slice().into(),
        key if m.control && !m.platform => {
            // Ctrl+letter and the punctuation that has a control code
            let c = key.chars().next()?;
            if key.chars().count() != 1 {
                return None;
            }
            let code = match c {
                'a'..='z' => c as u8 - b'a' + 1,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '7' | '/' => 0x1f,
                '2' | '@' => 0,
                '8' => 0x7f,
                _ => return None,
            };
            alt(&[code])
        }
        _ => {
            let text = ks.key_char.clone().or_else(|| (ks.key == "space").then(|| " ".to_string()))?;
            if m.platform {
                return None;
            }
            alt(text.as_bytes())
        }
    };
    Some(out)
}

/// xterm's colours for indexes 0–255 (the first 16 as a palette that reads on light and dark
/// backgrounds).
fn default_rgb(ix: usize, dark: bool) -> Rgb {
    const DARK: [(u8, u8, u8); 16] = [
        (0x1e, 0x1e, 0x1e),
        (0xe0, 0x6c, 0x75),
        (0x98, 0xc3, 0x79),
        (0xe5, 0xc0, 0x7b),
        (0x61, 0xaf, 0xef),
        (0xc6, 0x78, 0xdd),
        (0x56, 0xb6, 0xc2),
        (0xd4, 0xd4, 0xd4),
        (0x76, 0x76, 0x76),
        (0xff, 0x7b, 0x86),
        (0xb5, 0xe8, 0x90),
        (0xff, 0xd8, 0x8c),
        (0x82, 0xc8, 0xff),
        (0xe0, 0x9a, 0xf5),
        (0x7c, 0xd4, 0xdf),
        (0xff, 0xff, 0xff),
    ];
    const LIGHT: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xc4, 0x1a, 0x16),
        (0x1f, 0x7a, 0x1f),
        (0x9a, 0x6b, 0x00),
        (0x1d, 0x5f, 0xc4),
        (0x8e, 0x3c, 0xb0),
        (0x0e, 0x7c, 0x86),
        (0x5c, 0x5c, 0x5c),
        (0x6e, 0x6e, 0x6e),
        (0xe0, 0x32, 0x2d),
        (0x2d, 0x9a, 0x2d),
        (0xb5, 0x85, 0x00),
        (0x2f, 0x7b, 0xe8),
        (0xa8, 0x55, 0xc9),
        (0x13, 0x96, 0xa3),
        (0x2a, 0x2a, 0x2a),
    ];
    let rgb = |(r, g, b): (u8, u8, u8)| Rgb { r, g, b };
    match ix {
        0..=15 => rgb(if dark { DARK[ix] } else { LIGHT[ix] }),
        16..=231 => {
            let i = ix - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            Rgb { r: level(i / 36), g: level((i / 6) % 6), b: level(i % 6) }
        }
        232..=255 => {
            let v = (8 + (ix - 232) * 10) as u8;
            Rgb { r: v, g: v, b: v }
        }
        _ => rgb(if dark { (0xd4, 0xd4, 0xd4) } else { (0x20, 0x20, 0x20) }),
    }
}

fn hsla_of(c: Rgb) -> Hsla {
    Rgba { r: c.r as f32 / 255., g: c.g as f32 / 255., b: c.b as f32 / 255., a: 1. }.into()
}

/// A colour of the grid as GPUI paints it: None for the default background (the pane's own).
fn resolve(c: Color, colors: &alacritty_terminal::term::color::Colors, p: &Palette, bold: bool) -> Option<Hsla> {
    let ix = |i: usize| Some(hsla_of(colors[i].unwrap_or_else(|| default_rgb(i, p.dark))));
    match c {
        Color::Spec(rgb) => Some(hsla_of(rgb)),
        Color::Indexed(i) => {
            let i = i as usize;
            // bold text in the first 8 colours shows in their bright versions, as in xterm
            ix(if bold && i < 8 { i + 8 } else { i })
        }
        Color::Named(n) => match n {
            NamedColor::Foreground | NamedColor::BrightForeground => Some(colors[NamedColor::Foreground].map(hsla_of).unwrap_or(p.text)),
            NamedColor::DimForeground => Some(p.text_secondary),
            NamedColor::Background => colors[NamedColor::Background].map(hsla_of),
            NamedColor::Cursor => Some(p.accent),
            n if (n as usize) < 16 => ix(if bold && (n as usize) < 8 { n as usize + 8 } else { n as usize }),
            // the dim versions of the 8 colours
            n => {
                let base = (n as usize).saturating_sub(NamedColor::DimBlack as usize).min(7);
                ix(base).map(|c| Hsla { a: 0.7, ..c })
            }
        },
    }
}

/// A cell of the grid: the monospace font's advance, and its line height.
pub fn cell_size(window: &Window) -> Size<Pixels> {
    let font = Font { family: mono_family(), ..Font::default() };
    let font_id = window.text_system().resolve_font(&font);
    let w = window.text_system().advance(font_id, px(FONT_SIZE), 'M').map(|s| s.width).unwrap_or(px(8.));
    size(w, px((FONT_SIZE * 1.35).round()))
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let font = Font { family: mono_family(), ..Font::default() };
        let font_size = px(FONT_SIZE);
        let cell = cell_size(window);
        let focused = self.focus.is_focused(window);

        let term = self.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let cursor = content.cursor;
        let selection = content.selection;
        let colors = content.colors;
        let rows_n = self.size.rows;
        let mut rows: Vec<(String, Vec<TextRun>)> = vec![(String::new(), Vec::new()); rows_n];
        let bold_font = Font { weight: FontWeight::BOLD, ..font.clone() };
        let italic_font = Font { style: FontStyle::Italic, ..font.clone() };
        let bold_italic = Font { weight: FontWeight::BOLD, style: FontStyle::Italic, ..font.clone() };
        for ind in content.display_iter {
            let row = ind.point.line.0 + offset;
            if row < 0 || row as usize >= rows_n {
                continue;
            }
            let c = &ind.cell;
            if c.flags.contains(Flags::WIDE_CHAR_SPACER) || c.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let bold = c.flags.contains(Flags::BOLD);
            let mut fg = resolve(c.fg, colors, &p, bold).unwrap_or(p.text);
            let mut bg = resolve(c.bg, colors, &p, false);
            if c.flags.contains(Flags::INVERSE) {
                let old_fg = fg;
                fg = bg.unwrap_or(p.content_bg);
                bg = Some(old_fg);
            }
            if c.flags.contains(Flags::DIM) {
                fg.a *= 0.7;
            }
            if selection.is_some_and(|s| s.contains(ind.point)) {
                bg = Some(p.selection);
            }
            // a block cursor: the cell in reverse (an outline-free bar or underline is drawn
            // the same way, so the cursor is always visible)
            let at_cursor = ind.point == cursor.point && cursor.shape != CursorShape::Hidden && offset == 0;
            if at_cursor && focused {
                bg = Some(p.text);
                fg = p.content_bg;
            } else if at_cursor {
                bg = Some(p.text_tertiary);
            }
            if c.flags.contains(Flags::HIDDEN) {
                fg = bg.unwrap_or(p.content_bg);
            }
            let mut text = String::new();
            text.push(if c.c == '\0' || c.c == '\t' { ' ' } else { c.c });
            if let Some(zw) = c.zerowidth() {
                text.extend(zw.iter());
            }
            let font = match (bold, c.flags.contains(Flags::ITALIC)) {
                (true, true) => bold_italic.clone(),
                (true, false) => bold_font.clone(),
                (false, true) => italic_font.clone(),
                (false, false) => font.clone(),
            };
            let underline = c.flags.intersects(Flags::ALL_UNDERLINES).then(|| UnderlineStyle { thickness: px(1.), color: Some(fg), wavy: c.flags.contains(Flags::UNDERCURL) });
            let strike = c.flags.contains(Flags::STRIKEOUT).then(|| StrikethroughStyle { thickness: px(1.), color: Some(fg) });
            let (line, runs) = &mut rows[row as usize];
            line.push_str(&text);
            // one run per stretch of cells that look alike
            match runs.last_mut() {
                Some(r) if r.color == fg && r.background_color == bg && r.font == font && r.underline == underline && r.strikethrough == strike => r.len += text.len(),
                _ => runs.push(TextRun { len: text.len(), font, color: fg, background_color: bg, underline, strikethrough: strike }),
            }
        }
        drop(term);

        let me = cx.entity().downgrade();
        let probe = canvas(
            move |bounds, _, cx| {
                let _ = me.update(cx, |t, cx| {
                    let before = t.size;
                    t.set_bounds(bounds, cell);
                    if t.size != before {
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let lines = rows.into_iter().map(|(text, runs)| {
            let el: AnyElement = if text.is_empty() {
                div().h(cell.height).into_any_element()
            } else {
                div().h(cell.height).whitespace_nowrap().child(StyledText::new(text).with_runs(runs)).into_any_element()
            };
            el
        });

        div()
            .id("terminal-grid")
            .key_context("TerminalPane")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .overflow_hidden()
            .font_family(mono_family())
            .text_size(font_size)
            .line_height(cell.height)
            .text_color(p.text)
            .cursor_text()
            .on_scroll(cx.listener(|t, ev: &ScrollEvent, _, cx| t.on_scroll(ev, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|t, ev: &MouseDownEvent, window, cx| {
                    window.focus(&t.focus, cx);
                    let (point, side) = t.cell_at(ev.position);
                    let kind = match ev.click_count {
                        2 => SelectionType::Semantic,
                        3 => SelectionType::Lines,
                        _ => SelectionType::Simple,
                    };
                    let mut sel = Selection::new(kind, point, side);
                    if kind != SelectionType::Simple {
                        sel.update(point, side);
                    }
                    t.term.lock().selection = Some(sel);
                    t.selecting = true;
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|t, ev: &MouseMoveEvent, _, cx| {
                if t.selecting && ev.pressed_button == Some(MouseButton::Left) {
                    let (point, side) = t.cell_at(ev.position);
                    if let Some(sel) = t.term.lock().selection.as_mut() {
                        sel.update(point, side);
                    }
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|t, _: &MouseUpEvent, _, cx| {
                    t.selecting = false;
                    let mut term = t.term.lock();
                    if term.selection.as_ref().is_some_and(|s| s.is_empty()) {
                        term.selection = None;
                    }
                    cx.notify();
                }),
            )
            // middle click pastes, as in other Linux terminals
            .on_mouse_down(MouseButton::Middle, cx.listener(|t, _: &MouseDownEvent, _, cx| t.paste(cx)))
            .child(probe)
            .child(div().flex().flex_col().children(lines))
    }
}

#[cfg(test)]
mod tests {
    use super::{Keystroke, Modifiers, Rgb, default_rgb, key_bytes};

    fn ks(key: &str, ch: Option<&str>, ctrl: bool, alt: bool, shift: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers { control: ctrl, alt, shift, ..Default::default() },
            key: key.into(),
            key_char: ch.map(Into::into),
        }
    }

    fn bytes(k: Keystroke, app: bool) -> Vec<u8> {
        key_bytes(&k, app).map(|b| b.into_owned()).unwrap_or_default()
    }

    #[test]
    fn keys() {
        assert_eq!(bytes(ks("a", Some("a"), false, false, false), false), b"a");
        assert_eq!(bytes(ks("a", Some("A"), false, false, true), false), b"A");
        assert_eq!(bytes(ks("c", None, true, false, false), false), b"\x03");
        assert_eq!(bytes(ks("w", None, true, false, false), false), b"\x17");
        assert_eq!(bytes(ks("b", Some("b"), false, true, false), false), b"\x1bb");
        assert_eq!(bytes(ks("enter", None, false, false, false), false), b"\r");
        assert_eq!(bytes(ks("backspace", None, false, false, false), false), b"\x7f");
        assert_eq!(bytes(ks("up", None, false, false, false), false), b"\x1b[A");
        assert_eq!(bytes(ks("up", None, false, false, false), true), b"\x1bOA");
        assert_eq!(bytes(ks("left", None, true, false, false), false), b"\x1b[1;5D");
        assert_eq!(bytes(ks("delete", None, false, false, false), false), b"\x1b[3~");
        assert_eq!(bytes(ks("f1", None, false, false, false), false), b"\x1bOP");
        assert_eq!(bytes(ks("f5", None, false, false, false), false), b"\x1b[15~");
        assert_eq!(bytes(ks("tab", None, false, false, true), false), b"\x1b[Z");
        assert_eq!(bytes(ks("space", Some(" "), false, false, false), false), b" ");
        assert!(key_bytes(&ks("shift", None, false, false, true), false).is_none());
    }

    #[test]
    fn palette() {
        assert_eq!(default_rgb(16, true), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(default_rgb(231, true), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(default_rgb(232, true), Rgb { r: 8, g: 8, b: 8 });
    }
}

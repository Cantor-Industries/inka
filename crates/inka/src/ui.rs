// inka UI: styling, verbosity, and terminal progress for the CLI.
//
// Deno-flavored. Color comes from `deno_terminal` (`NO_COLOR` / `FORCE_COLOR`)
// but is additionally gated on a TTY so piped/CI output stays plain and
// machine-parseable. Verbosity defaults to `Info`; `INK_LOG`/`INK_DEBUG` and
// the `-q`/`-v` flags adjust it.
//
// Diagnostics (errors/warnings) go to stderr; report output (e.g. `doctor`)
// goes to stdout. Progress is always stderr.
#![allow(dead_code)]

use std::fmt::Display;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use deno_terminal::colors;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub(crate) fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

pub(crate) fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        3 => Level::Debug,
        _ => Level::Trace,
    }
}

/// Resolve color and verbosity once, from the environment. Call before any
/// command runs.
pub(crate) fn init_from_env() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let forced = colors::force_color();
        let mut color = colors::use_color();
        match std::env::var("INK_LOG_STYLE").as_deref() {
            Ok("always") => color = true,
            Ok("never") => color = false,
            _ => {
                // `FORCE_COLOR` overrides the TTY gate; otherwise color is only
                // enabled when a stream is a terminal.
                if !forced && !(deno_terminal::is_stdout_tty() || deno_terminal::is_stderr_tty()) {
                    color = false;
                }
            }
        }
        colors::set_use_color(color);

        if let Ok(spec) = std::env::var("INK_LOG") {
            if let Some(l) = parse_level(&spec) {
                set_level(l);
            }
        }
        if std::env::var_os("INKA_DEBUG").is_some() {
            set_level(Level::Debug);
        }
    });
}

fn parse_level(s: &str) -> Option<Level> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "error" => Level::Error,
        "warn" | "warning" => Level::Warn,
        "info" => Level::Info,
        "debug" => Level::Debug,
        "trace" => Level::Trace,
        _ => return None,
    })
}

/// Accept `-q`/`--quiet`/`-v`/`--verbose`; returns true when consumed.
pub(crate) fn apply_verbosity_flag(arg: &str) -> bool {
    match arg {
        "-q" | "--quiet" => {
            set_level(Level::Warn);
            true
        }
        "-v" | "--verbose" => {
            set_level(Level::Debug);
            true
        }
        _ => false,
    }
}

pub(crate) fn log_error(msg: impl Display) {
    let text = msg.to_string();
    let mut lines = text.lines();
    match lines.next() {
        Some(first) => eprintln!("{}: {}", colors::red_bold("error"), first),
        None => {
            eprintln!("{}:", colors::red_bold("error"));
            return;
        }
    }
    // Continuation lines (e.g. a JS stack) are dimmed as-is.
    for line in lines {
        eprintln!("{}", colors::gray(line));
    }
}

pub(crate) fn warn(msg: impl Display) {
    eprintln!("{}: {}", colors::yellow_bold("warning"), msg);
}

pub(crate) fn info(msg: impl Display) {
    if level() >= Level::Info {
        eprintln!("{}: {}", colors::cyan("info"), msg);
    }
}

pub(crate) fn success(msg: impl Display) {
    if level() >= Level::Info {
        eprintln!("{} {}", colors::green("✓"), msg);
    }
}

pub(crate) fn hint(msg: impl Display) {
    if level() >= Level::Info {
        eprintln!("  {} {}", colors::cyan("hint:"), msg);
    }
}

pub(crate) fn detail(msg: impl Display) {
    if level() >= Level::Info {
        eprintln!("  {}", colors::gray(msg));
    }
}

pub(crate) fn debug(msg: impl Display) {
    if level() >= Level::Debug {
        eprintln!("{}: {}", colors::gray("debug"), colors::gray(msg));
    }
}

// ---- report (stdout) --------------------------------------------------------
// Structured command output (doctor/build/update). One vocabulary so every
// command reads the same; diagnostics above stay on stderr.

/// `inka <name>  <version>` preceded by a blank line.
pub(crate) fn title(name: &str) {
    println!();
    println!(
        "{} {}",
        colors::bold(format!("inka {name}")),
        colors::gray(env!("CARGO_PKG_VERSION")),
    );
}

/// A bold section header preceded by a blank line.
pub(crate) fn section(name: &str) {
    println!();
    println!("{}", colors::bold(name));
}

const LABEL_WIDTH: usize = 14;

pub(crate) fn ok(label: &str, value: impl Display) {
    println!("  {} {label:<LABEL_WIDTH$} {value}", colors::green("✓"));
}

pub(crate) fn warn_row(label: &str, value: impl Display) {
    println!("  {} {label:<LABEL_WIDTH$} {value}", colors::yellow("!"));
}

pub(crate) fn bad_row(label: &str, value: impl Display) {
    println!("  {} {label:<LABEL_WIDTH$} {value}", colors::red("✗"));
}

pub(crate) fn doing(label: &str, value: impl Display) {
    println!("  {} {label:<LABEL_WIDTH$} {value}", colors::cyan("→"));
}

pub(crate) fn row(label: &str, value: impl Display) {
    println!("  {label:<LABEL_WIDTH$} {value}");
}

pub(crate) fn status_ok(msg: impl Display) {
    println!();
    println!("{} {}", colors::green("✓"), colors::green(msg));
}

pub(crate) fn status_warn(msg: impl Display) {
    println!();
    println!("{} {}", colors::yellow("!"), colors::yellow(msg));
}

pub(crate) fn status_bad(msg: impl Display) {
    println!();
    println!("{} {}", colors::red("✗"), colors::red(msg));
}

/// Human-readable byte size (`1.2MiB`, `512B`).
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1}{}", UNITS[i])
}

fn human_elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{}s", s / 60, s % 60)
    }
}

/// Visible column count of a string, ignoring ANSI SGR sequences.
fn visible_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut w = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'm' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        if b & 0xC0 != 0x80 {
            w += 1;
        }
        i += 1;
    }
    w
}

/// Terminal width: `ioctl(TIOCGWINSZ)` on stderr, then `$COLUMNS`, then `None`.
#[cfg(unix)]
fn terminal_width() -> Option<usize> {
    let mut ws = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    let rc = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, ws.as_mut_ptr()) };
    if rc == 0 {
        let ws = unsafe { ws.assume_init() };
        if ws.ws_col > 0 {
            return Some(ws.ws_col as usize);
        }
    }
    if let Ok(cols) = std::env::var("COLUMNS") {
        if let Ok(n) = cols.trim().parse::<usize>() {
            if n > 0 {
                return Some(n);
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn terminal_width() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse().ok())
        .filter(|n| *n > 0)
}

const SPINNER: [&str; 6] = ["▰▱▱▱▱▱", "▰▰▱▱▱▱", "▰▰▰▱▱▱", "▰▰▰▰▱▱", "▰▰▰▰▰▱", "▰▰▰▰▰▰"];

/// A single-line terminal progress bar for downloads. On a non-TTY (or below
/// `Info`) it emits one `info:` line and draws nothing. The line is always
/// cleared on drop, so a failed download never leaves bar residue.
pub(crate) struct Progress {
    label: String,
    total: Option<u64>,
    pos: u64,
    enabled: bool,
    started: Instant,
    tick: usize,
    prev_width: usize,
    finished: bool,
}

impl Progress {
    pub(crate) fn download(name: &str, total: Option<u64>) -> Progress {
        let enabled = level() >= Level::Info && deno_terminal::is_stderr_tty();
        if !enabled {
            match total {
                Some(t) => info(format!("downloading {name} ({})", human_size(t))),
                None => info(format!("downloading {name}")),
            }
        }
        Progress {
            label: format!("Downloading {name}"),
            total,
            pos: 0,
            enabled,
            started: Instant::now(),
            tick: 0,
            prev_width: 0,
            finished: false,
        }
    }

    pub(crate) fn set(&mut self, pos: u64) {
        self.pos = pos;
        self.draw();
    }

    pub(crate) fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.draw();
    }

    pub(crate) fn finish(&mut self) {
        if !self.enabled || self.finished {
            return;
        }
        if let Some(total) = self.total {
            self.pos = self.pos.max(total);
        }
        let line = self.render();
        self.write_line(&line, true);
        self.finished = true;
        self.prev_width = 0;
    }

    fn draw(&mut self) {
        if !self.enabled || self.finished {
            return;
        }
        let line = self.render();
        self.write_line(&line, false);
    }

    fn write_line(&mut self, line: &str, newline: bool) {
        let width = visible_width(line);
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{line}");
        if width < self.prev_width {
            let _ = write!(err, "{}", " ".repeat(self.prev_width - width));
        }
        if newline {
            let _ = writeln!(err);
        }
        let _ = err.flush();
        self.prev_width = if newline { 0 } else { width };
    }

    fn render(&self) -> String {
        let elapsed = human_elapsed(self.started.elapsed());
        let width = terminal_width().unwrap_or(80);

        let (bar, plain_suffix) = match self.total {
            Some(total) if total > 0 => {
                let ratio = (self.pos as f64 / total as f64).clamp(0.0, 1.0);
                let pct = (ratio * 100.0).floor() as u64;
                let plain = format!(
                    "{}/{} {pct}%  {elapsed}",
                    human_size(self.pos),
                    human_size(total)
                );
                // Size the bar from the widest possible suffix so its length
                // stays constant as the byte count and percent grow.
                let widest = format!("{t}/{t} 100%  59m59s", t = human_size(total));
                let overhead = self.label.chars().count() + 2 + 2 + 2 + widest.chars().count();
                let bar_len = width.saturating_sub(overhead).clamp(10, 40);
                (self.determinate_bar(ratio, bar_len), plain)
            }
            _ => {
                let frame = SPINNER[self.tick % SPINNER.len()];
                let plain = format!("{}  {elapsed}", human_size(self.pos));
                (colors::cyan(frame).to_string(), plain)
            }
        };
        format!("{}  [{bar}]  {}", self.label, colors::gray(plain_suffix))
    }

    fn determinate_bar(&self, ratio: f64, bar_len: usize) -> String {
        let completed = ((bar_len as f64) * ratio).round() as usize;
        if completed >= bar_len {
            colors::cyan("#".repeat(bar_len)).to_string()
        } else if completed == 0 {
            colors::intense_blue("-".repeat(bar_len)).to_string()
        } else {
            format!(
                "{}{}",
                colors::cyan(format!("{}>", "#".repeat(completed - 1))),
                colors::intense_blue("-".repeat(bar_len - completed))
            )
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if self.enabled && !self.finished {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r{}\r", " ".repeat(self.prev_width));
            let _ = err.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_rounds_units() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1024), "1.0KiB");
        assert_eq!(human_size(1536), "1.5KiB");
        assert_eq!(human_size(1024 * 1024), "1.0MiB");
    }

    #[test]
    fn visible_width_ignores_ansi() {
        assert_eq!(visible_width("plain"), 5);
        assert_eq!(visible_width("\x1b[32mgreen\x1b[0m"), 5);
        assert_eq!(visible_width("a\x1b[1mб\x1b[0m"), 2);
    }

    #[test]
    fn parse_level_accepts_names() {
        assert_eq!(parse_level("error"), Some(Level::Error));
        assert_eq!(parse_level("WARN"), Some(Level::Warn));
        assert_eq!(parse_level(" debug "), Some(Level::Debug));
        assert_eq!(parse_level("nope"), None);
    }

    #[test]
    fn verbosity_flags_update_level() {
        set_level(Level::Info);
        assert!(apply_verbosity_flag("-q"));
        assert_eq!(level(), Level::Warn);
        assert!(apply_verbosity_flag("--verbose"));
        assert_eq!(level(), Level::Debug);
        assert!(!apply_verbosity_flag("--allow-read"));
        set_level(Level::Info);
    }

    fn progress(total: Option<u64>, pos: u64) -> Progress {
        Progress {
            label: "Downloading x".to_string(),
            total,
            pos,
            enabled: true,
            started: Instant::now(),
            tick: 3,
            prev_width: 0,
            finished: false,
        }
    }

    #[test]
    fn progress_renders_percent_and_bar() {
        let line = progress(Some(100), 50).render();
        assert!(line.contains("Downloading x"), "{line}");
        assert!(line.contains("50%"), "{line}");
        assert!(line.contains('[') && line.contains(']'), "{line}");
    }

    #[test]
    fn progress_indeterminate_uses_spinner() {
        let line = progress(None, 0).render();
        assert!(line.contains("Downloading x"), "{line}");
        assert!(line.contains(SPINNER[3]), "{line}");
    }
}

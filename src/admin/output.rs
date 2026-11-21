//! Accessible terminal output helpers for the admin CLI.
//!
//! Status is always conveyed by an explicit word (e.g. `OK`, `FAILED`) so it
//! survives a monochrome terminal and a screen reader. Colour and decorative
//! frames are only emitted when stdout is an interactive TTY and the user has
//! not opted out via `NO_COLOR` or `TERM=dumb`.

use std::io::IsTerminal;

/// Whether colour escape sequences may be emitted.
pub fn use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(std::env::var("TERM").as_deref(), Ok("dumb")) {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Whether decorative box-drawing / spinner frames may be emitted.
pub fn use_decoration() -> bool {
    use_color()
}

/// A status word plus its optional colour code.
#[derive(Clone, Copy)]
pub enum Status {
    Ok,
    Failed,
    Info,
}

impl Status {
    /// The explicit, screen-reader friendly status word.
    pub fn word(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Failed => "FAILED",
            Status::Info => "INFO",
        }
    }

    fn color_code(self) -> &'static str {
        match self {
            Status::Ok => "\x1b[32m",
            Status::Failed => "\x1b[31m",
            Status::Info => "\x1b[34m",
        }
    }
}

/// Format a status word, colouring it only when colour is enabled.
pub fn status_word(status: Status) -> String {
    if use_color() {
        format!("{}{}\x1b[0m", status.color_code(), status.word())
    } else {
        status.word().to_string()
    }
}

/// Print a status line: `[OK] message` (plain when not a TTY).
pub fn status_line(status: Status, message: &str) {
    println!("[{}] {}", status_word(status), message);
}

/// Print a decorative rule, only when decoration is enabled.
pub fn rule() {
    if use_decoration() {
        println!("────────────────────────────────────────");
    }
}

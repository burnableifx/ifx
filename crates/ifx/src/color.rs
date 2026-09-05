//! Minimal ANSI styling that honours `NO_COLOR`, `CLICOLOR_FORCE`, and whether stdout
//! is a terminal. Methods return `String`, so styled text composes with `format!`.

use std::fmt::Display;
use std::sync::OnceLock;

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Decide once, before any output. Later calls are ignored.
pub fn init(enabled: bool) {
    let _ = ENABLED.set(enabled);
}

/// Auto-detect: `CLICOLOR_FORCE` wins, then `NO_COLOR`, then "is stdout a tty".
pub fn auto() -> bool {
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
        return true;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

pub fn enabled() -> bool {
    *ENABLED.get_or_init(auto)
}

fn wrap(s: impl Display, code: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// `"text".green()`, `x.to_string().bold()`, chainable.
pub trait Paint: Display + Sized {
    fn red(&self) -> String {
        wrap(self, "31")
    }
    fn green(&self) -> String {
        wrap(self, "32")
    }
    fn yellow(&self) -> String {
        wrap(self, "33")
    }
    fn magenta(&self) -> String {
        wrap(self, "35")
    }
    fn cyan(&self) -> String {
        wrap(self, "36")
    }
    fn bold(&self) -> String {
        wrap(self, "1")
    }
    fn dimmed(&self) -> String {
        wrap(self, "2")
    }
}

impl<T: Display> Paint for T {}

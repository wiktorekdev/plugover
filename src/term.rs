//! Terminal output. Print through `anstream`'s `println!` so colors appear only on a terminal and
//! respect NO_COLOR and CLICOLOR.

use anstream::println;
use anstyle::{Ansi256Color, AnsiColor, Color, Style};
use std::{
    fmt::Display,
    sync::atomic::{AtomicUsize, Ordering},
};

/// How many `warn` lines were printed.
pub static WARNINGS: AtomicUsize = AtomicUsize::new(0);

pub const BOLD: Style = Style::new().bold();
pub const DIM: Style = Style::new().dimmed();
pub const GOOD: Style = AnsiColor::Green.on_default();
pub const WARN: Style = AnsiColor::Yellow.on_default();
pub const BAD: Style = AnsiColor::Red.on_default();
/// Violet, like the punktfunk console and the installer.
pub const ACCENT: Style = Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(141))));

pub const CLAP: clap::builder::Styles = clap::builder::Styles::styled()
    .header(BOLD.underline())
    .usage(BOLD.underline())
    .literal(ACCENT.bold())
    .placeholder(ACCENT)
    .error(BAD.bold())
    .valid(GOOD)
    .invalid(WARN);

pub fn paint(style: Style, text: impl Display) -> String {
    format!("{style}{text}{style:#}")
}

/// Pads to `width` columns before painting, so escape codes do not throw columns off.
pub fn cell(style: Style, text: &str, width: usize) -> String {
    format!("{}{}", paint(style, text), " ".repeat(width.saturating_sub(text.chars().count())))
}

pub fn widest<'a>(texts: impl IntoIterator<Item = &'a str>) -> usize {
    texts.into_iter().map(|t| t.chars().count()).max().unwrap_or(0)
}

pub fn ok(what: &str, detail: impl Display) {
    println!("  {} {what}{}", paint(GOOD.bold(), "✓"), after(DIM, detail));
}

/// " detail", painted, or nothing for an empty detail.
pub fn after(style: Style, detail: impl Display) -> String {
    let d = detail.to_string();
    if d.is_empty() { d } else { format!(" {}", paint(style, d)) }
}

pub fn warn(what: &str, detail: impl Display, hint: Option<&str>) {
    WARNINGS.fetch_add(1, Ordering::Relaxed);
    println!("  {} {what}{}", paint(WARN.bold(), "!"), after(WARN, detail));
    hint_line(hint);
}

pub fn fail(what: &str, detail: impl Display, hint: Option<&str>) {
    println!("  {} {what}{}", paint(BAD.bold(), "✗"), after(BAD, detail));
    hint_line(hint);
}

fn hint_line(hint: Option<&str>) {
    if let Some(h) = hint {
        println!("    {} {h}", paint(DIM, "→"));
    }
}

/// A command, styled like the ones in clap's help.
pub fn cmd(c: &str) -> String {
    paint(ACCENT, c)
}

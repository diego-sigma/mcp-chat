use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use nu_ansi_term::Color;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    /// Suppress every tool-related line.
    Quiet,
    /// Tool name + spinner while we wait. No args, no raw result.
    Normal,
    /// Full `→ name(args)` / `← result` pair (the old verbose default).
    Verbose,
}

#[derive(Clone, Copy)]
pub struct Ui {
    pub verbosity: Verbosity,
}

/// Handle returned by `Ui::tool_call_begin`; pass to `tool_call_end` once the
/// MCP call returns. Holds an optional spinner so calls in `Quiet` mode are
/// free.
pub struct ToolCallSpan {
    spinner: Option<ProgressBar>,
}

impl Ui {
    pub fn new(verbosity: Verbosity) -> Self {
        Self { verbosity }
    }

    pub fn tool_call_begin(&self, name: &str, args: &str) -> ToolCallSpan {
        match self.verbosity {
            Verbosity::Quiet => ToolCallSpan { spinner: None },
            Verbosity::Verbose => {
                let arrow = Color::Cyan.bold().paint("→");
                let name_paint = Color::Cyan.bold().paint(name);
                let args_paint = Color::Yellow.dimmed().paint(args);
                eprintln!("{arrow} {name_paint}({args_paint})");
                ToolCallSpan { spinner: None }
            }
            Verbosity::Normal => {
                let prefix = format!(
                    "{} {} ",
                    Color::Cyan.bold().paint("→"),
                    Color::Cyan.bold().paint(name)
                );
                let pb = ProgressBar::new_spinner();
                pb.set_style(
                    ProgressStyle::with_template("{prefix}{spinner:.cyan} {msg}")
                        .unwrap()
                        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
                );
                pb.set_prefix(prefix);
                pb.set_message("…");
                pb.enable_steady_tick(Duration::from_millis(80));
                ToolCallSpan { spinner: Some(pb) }
            }
        }
    }

    pub fn tool_call_end(&self, span: ToolCallSpan, result: &str, is_error: bool) {
        if let Some(pb) = span.spinner {
            let mark = if is_error {
                Color::Red.bold().paint("✗").to_string()
            } else {
                Color::Green.bold().paint("✓").to_string()
            };
            // `finish_with_message` clears the spinner and prints prefix + mark.
            pb.finish_with_message(mark);
        }
        match self.verbosity {
            Verbosity::Quiet => {}
            Verbosity::Normal => {
                if is_error {
                    let label = Color::Red.bold().paint("  error:");
                    let body = Color::Red.dimmed().paint(truncate(result, 4_000));
                    eprintln!("{label} {body}");
                }
            }
            Verbosity::Verbose => {
                let arrow = Color::Green.bold().paint("←");
                let trimmed = truncate(result, 4_000);
                let body = if is_error {
                    Color::Red.dimmed().paint(trimmed).to_string()
                } else {
                    Color::Yellow.dimmed().paint(trimmed).to_string()
                };
                eprintln!("{arrow} {body}");
            }
        }
    }

    pub fn assistant(&self, text: &str) {
        let label = Color::Magenta.bold().paint("assistant");
        println!("{label}:");
        termimad::print_text(text);
    }

    pub fn error(&self, text: &str) {
        let label = Color::Red.bold().paint("error");
        eprintln!("{label}: {text}");
    }

    pub fn system_info(&self, text: &str) {
        let dim = Color::DarkGray.italic().paint(text);
        eprintln!("{dim}");
    }
}

/// Push the cursor up `lines` rows by printing that many newlines then moving
/// back up — leaves `lines` blank rows visible below the cursor so the prompt
/// isn't pinned to the very bottom of the terminal.
///
/// Uses `\x1b[NF` ("Cursor Previous Line"), which moves up N rows AND resets
/// the column to 0 in one step. The simpler `\x1b[NA` ("Cursor Up") keeps the
/// column intact, so if `\n` doesn't already reset the column (which it doesn't
/// when the terminal is in raw mode) the prompt ends up indented.
pub fn pad_screen_bottom(lines: usize) {
    if lines == 0 {
        return;
    }
    use std::io::Write;
    print!("{}\x1b[{}F", "\n".repeat(lines), lines);
    let _ = std::io::stdout().flush();
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut out = s[..max].to_string();
        out.push_str("… [truncated]");
        out
    }
}

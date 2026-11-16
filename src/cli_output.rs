use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

#[derive(Default)]
pub enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

impl Color {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            _ => Err("--color expects auto, always or never".into()),
        }
    }

    fn enabled(&self) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => {
                std::io::stderr().is_terminal()
                    && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                    && std::env::var_os("TERM").is_none_or(|v| v != "dumb")
            }
        }
    }

    fn paint(&self, text: &str, style: &str) -> String {
        if self.enabled() {
            format!("\x1b[{style}m{text}\x1b[0m")
        } else {
            text.into()
        }
    }

    pub fn status(&self, label: &str, path: &Path, elapsed: Duration) {
        eprintln!(
            "{} {} in {:.3}s",
            self.paint(label, "1;32"),
            path.display(),
            elapsed.as_secs_f64()
        );
    }

    pub fn error(&self, error: &str) {
        let text = if error.starts_with("error[") || error.starts_with("error:") {
            error.to_owned()
        } else {
            format!("error: {error}")
        };
        let mut suggestion_preview = false;
        for line in text.lines() {
            if line.starts_with("error[") || line.starts_with("error:") {
                suggestion_preview = false;
                if let Some((label, message)) = line.split_once(": ") {
                    eprintln!(
                        "{}: {}",
                        self.paint(label, "1;31"),
                        self.paint(message, "1")
                    );
                    continue;
                }
            } else if let Some(location) = line.strip_prefix(" --> ") {
                eprintln!(" {} {location}", self.paint("-->", "1;36"));
                continue;
            } else if let Some(note) = line.strip_prefix(" note: ") {
                suggestion_preview = false;
                eprintln!(" {} {note}", self.paint("note:", "1;36"));
                continue;
            } else if let Some(help) = line.strip_prefix(" help: ") {
                suggestion_preview = true;
                eprintln!(" {} {help}", self.paint("help:", "1;32"));
                continue;
            } else if let Some((gutter, source)) = line.split_once('|')
                && gutter.chars().all(|c| c.is_ascii_digit() || c == ' ')
            {
                let source = if gutter.trim().is_empty() && source.trim_start().starts_with('^') {
                    let style = if suggestion_preview { "1;32" } else { "1;31" };
                    suggestion_preview = false;
                    self.paint(source, style)
                } else if suggestion_preview && !gutter.trim().is_empty() {
                    self.paint(source, "32")
                } else {
                    source.into()
                };
                eprintln!("{}{source}", self.paint(&format!("{gutter}|"), "1;36"));
                continue;
            }
            eprintln!("{line}");
        }
    }

    pub fn warning(&self, warning: &str) {
        for line in warning.lines() {
            if let Some((label, message)) = line.split_once(": ")
                && (label.starts_with("warning[") || label == "warning")
            {
                eprintln!(
                    "{}: {}",
                    self.paint(label, "1;33"),
                    self.paint(message, "1")
                );
            } else if let Some(location) = line.strip_prefix(" --> ") {
                eprintln!(" {} {location}", self.paint("-->", "1;36"));
            } else if let Some(note) = line.strip_prefix(" note: ") {
                eprintln!(" {} {note}", self.paint("note:", "1;36"));
            } else if let Some(help) = line.strip_prefix(" help: ") {
                eprintln!(" {} {help}", self.paint("help:", "1;32"));
            } else if let Some((gutter, source)) = line.split_once('|')
                && gutter.chars().all(|c| c.is_ascii_digit() || c == ' ')
            {
                let source = if gutter.trim().is_empty() && source.trim_start().starts_with('^') {
                    self.paint(source, "1;33")
                } else {
                    source.into()
                };
                eprintln!("{}{source}", self.paint(&format!("{gutter}|"), "1;36"));
            } else {
                eprintln!("{line}");
            }
        }
    }
}

pub fn preparation_error(error: String, action: &str, path: &Path, elapsed: Duration) -> String {
    let count = error
        .lines()
        .filter(|line| line.starts_with("error["))
        .count()
        .max(1);
    let error = if error.starts_with("error[") || error.starts_with("error:") {
        error
    } else {
        format!("error: {error}")
    };
    format!(
        "{error}\n\nerror: {action} failed for `{}` due to {count} error{} ({:.3}s)",
        path.display(),
        if count == 1 { "" } else { "s" },
        elapsed.as_secs_f64()
    )
}

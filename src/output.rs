//! JSON output: indented and colored on a terminal, compact with `--compact`.

use serde_json::Value;
use std::io::{IsTerminal, Write};

pub const RED: &str = "31";
pub const GREEN: &str = "32";
pub const YELLOW: &str = "33";
pub const BLUE: &str = "34";
pub const CYAN: &str = "36";
pub const DIM: &str = "2";
pub const BOLD: &str = "1";

fn color_allowed() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

pub fn stdout_color() -> bool {
    color_allowed() && std::io::stdout().is_terminal()
}

pub fn stderr_color() -> bool {
    color_allowed() && std::io::stderr().is_terminal()
}

pub fn paint(code: &str, text: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

#[derive(Clone, Copy)]
pub struct Style {
    pub compact: bool,
    pub color: bool,
}

impl Style {
    pub fn stdout(compact: bool) -> Self {
        Style {
            compact,
            color: stdout_color(),
        }
    }
}

pub fn print(value: &Value, style: Style) {
    let mut out = String::new();
    render(value, style, 0, &mut out);
    out.push('\n');
    let mut stdout = std::io::stdout().lock();
    // A closed pipe (`bias ... | head`) isn't an error worth reporting.
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

fn render(value: &Value, style: Style, depth: usize, out: &mut String) {
    let indent = |depth: usize, out: &mut String| {
        if !style.compact {
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
        }
    };
    match value {
        Value::Null => out.push_str(&paint(DIM, "null", style.color)),
        Value::Bool(b) => out.push_str(&paint(YELLOW, &b.to_string(), style.color)),
        Value::Number(n) => out.push_str(&paint(CYAN, &n.to_string(), style.color)),
        Value::String(_) => out.push_str(&paint(GREEN, &value.to_string(), style.color)),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                indent(depth + 1, out);
                render(item, style, depth + 1, out);
            }
            indent(depth, out);
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                indent(depth + 1, out);
                let key = Value::String(key.clone()).to_string();
                out.push_str(&paint(BLUE, &key, style.color));
                out.push_str(if style.compact { ":" } else { ": " });
                render(item, style, depth + 1, out);
            }
            indent(depth, out);
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(value: &Value, compact: bool) -> String {
        let mut out = String::new();
        render(
            value,
            Style {
                compact,
                color: false,
            },
            0,
            &mut out,
        );
        out
    }

    #[test]
    fn renders_like_serde() {
        let value = json!({"a": [1, {"b": null}], "c": "x\"y", "d": {}, "e": []});
        assert_eq!(text(&value, true), value.to_string());
        assert_eq!(
            text(&value, false),
            serde_json::to_string_pretty(&value).unwrap()
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later

//! Structured logging: one JSON object per line on stdout.
//!
//! Every line has an `"event"` field naming what happened; the other fields
//! say what was attempted, what was expected and what happened.

use std::fmt::Write as _;
use std::io::Write as _;

/// A field value.
#[derive(Clone, Copy)]
pub enum Value<'a> {
    Str(&'a str),
    U64(u64),
    /// Written as a `"0x..."` string, for addresses, offsets and registers.
    Hex(u64),
    Bool(bool),
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(s: &'a str) -> Self {
        Value::Str(s)
    }
}

impl<'a> From<&'a String> for Value<'a> {
    fn from(s: &'a String) -> Self {
        Value::Str(s)
    }
}

impl From<u64> for Value<'_> {
    fn from(n: u64) -> Self {
        Value::U64(n)
    }
}

impl From<u32> for Value<'_> {
    fn from(n: u32) -> Self {
        Value::U64(n.into())
    }
}

impl From<bool> for Value<'_> {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

fn push_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Format one log line (without the trailing newline).
pub fn format(event: &str, fields: &[(&str, Value)]) -> String {
    let mut out = String::from("{\"event\":");
    push_str(&mut out, event);
    for (key, value) in fields {
        out.push(',');
        push_str(&mut out, key);
        out.push(':');
        match value {
            Value::Str(s) => push_str(&mut out, s),
            Value::U64(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Hex(n) => {
                let _ = write!(out, "\"0x{n:x}\"");
            }
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        }
    }
    out.push('}');
    out
}

/// Write one log line to stdout. A failure to log is itself reported on
/// stderr rather than silently dropped.
pub fn emit(event: &str, fields: &[(&str, Value)]) {
    let line = format(event, fields);
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = writeln!(stdout, "{line}").and_then(|_| stdout.flush()) {
        eprintln!("grind log write failed ({e}): {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_strings_and_formats_values() {
        let line = format(
            "test",
            &[
                ("s", Value::Str("a\"b\\c\nd\u{1}")),
                ("n", Value::U64(42)),
                ("h", Value::Hex(0x1f)),
                ("b", Value::Bool(true)),
            ],
        );
        assert_eq!(
            line,
            r#"{"event":"test","s":"a\"b\\c\nd\u0001","n":42,"h":"0x1f","b":true}"#
        );
    }
}

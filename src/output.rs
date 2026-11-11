//! Rendering command results as human text or the `--json` envelope.
//!
//! JSON envelope (stdout):
//! `{ "ok": true, "data": ... }` or `{ "ok": false, "data"?: ..., "error": { code, message, hint? } }`

use serde::Serialize;
use serde_json::{Value, json};

use crate::error::{DeskError, ErrorJson};

/// The result of a command that ran. `failure` is set when part of it failed (e.g. one of several
/// devices); the data is still reported.
pub struct Report {
    pub data: Value,
    pub human: String,
    pub failure: Option<(ErrorJson, i32)>,
    /// The command already printed its output as it went (`watch`); only a failure is left.
    pub streamed: bool,
    /// The command printed everything itself, its error too (`desk stream`, whose stdout may
    /// carry binary data): only this exit code is left.
    pub printed: Option<i32>,
}

impl Report {
    pub fn new(data: &impl Serialize, human: String) -> Self {
        Report {
            data: serde_json::to_value(data).unwrap_or(Value::Null),
            human,
            failure: None,
            streamed: false,
            printed: None,
        }
    }

    /// A command that printed everything itself and exits with `code`.
    pub fn printed(code: i32) -> Self {
        Report {
            data: Value::Null,
            human: String::new(),
            failure: None,
            streamed: true,
            printed: Some(code),
        }
    }

    /// A command that printed its own output line by line, ending with `failure` or not.
    pub fn streamed(failure: Option<(ErrorJson, i32)>) -> Self {
        Report {
            data: Value::Null,
            human: String::new(),
            failure,
            streamed: true,
            printed: None,
        }
    }

    pub fn with_failure(mut self, failure: Option<(ErrorJson, i32)>) -> Self {
        self.failure = failure;
        self
    }
}

/// Print the result and return the process exit code.
pub fn emit(json_mode: bool, result: Result<Report, DeskError>) -> i32 {
    match result {
        Ok(report) => {
            if let Some(code) = report.printed {
                return code;
            }
            let code = report.failure.as_ref().map_or(0, |(_, c)| *c);
            if report.streamed {
                // Streams are JSON lines: the error, if any, is one more line.
                if let Some((err, _)) = &report.failure {
                    if json_mode {
                        println!("{}", json!({ "ok": false, "error": err }));
                    } else {
                        eprintln!("error: {}", err.message);
                        if let Some(hint) = &err.hint {
                            eprintln!("hint: {hint}");
                        }
                    }
                }
                return code;
            }
            if json_mode {
                let mut env = json!({ "ok": report.failure.is_none() });
                // Errors forwarded from the daemon have no data, like local errors.
                if !report.data.is_null() || report.failure.is_none() {
                    env["data"] = report.data;
                }
                if let Some((err, _)) = &report.failure {
                    env["error"] = json!(err);
                }
                print_json(&env);
            } else {
                print!("{}", report.human);
                // A single failure is already in the table; only summaries need an extra line.
                if let Some((err, _)) = &report.failure {
                    if !report.human.contains(&err.message) {
                        eprintln!("error: {}", err.message);
                    }
                    if let Some(hint) = &err.hint {
                        eprintln!("hint: {hint}");
                    }
                }
            }
            code
        }
        Err(e) => {
            emit_error(json_mode, &e);
            e.exit_code()
        }
    }
}

pub fn emit_error(json_mode: bool, e: &DeskError) {
    if json_mode {
        print_json(&json!({ "ok": false, "error": e.to_json() }));
    } else {
        eprintln!("error: {e}");
        if let Some(hint) = e.hint() {
            eprintln!("hint: {hint}");
        }
    }
}

fn print_json(v: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    );
}

/// Shorten `s` to at most `max` characters by cutting out the middle, keeping the (usually
/// distinctive) end: `switch.workbench_…_outlet_2`.
pub fn middle_ellipsis(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max || max < 5 {
        return s.to_string();
    }
    let head = (max - 1) / 3;
    let tail = max - 1 - head;
    let start: String = s.chars().take(head).collect();
    let end: String = s.chars().skip(len - tail).collect();
    format!("{start}…{end}")
}

/// Left-aligned text table; the last column is not padded.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut s = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                s.push_str(cell);
            } else {
                s.push_str(&format!("{cell:<w$}  ", w = widths[i]));
            }
        }
        s.trim_end().to_string() + "\n"
    };
    let mut out = line(headers.to_vec());
    for row in rows {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_ellipsis_keeps_the_end() {
        assert_eq!(middle_ellipsis("short", 10), "short");
        let s = "ha (entity=switch.workbench_power_strip_with_a_long_name_outlet_2)";
        let m = middle_ellipsis(s, 48);
        assert_eq!(m.chars().count(), 48);
        assert!(m.starts_with("ha (entity="));
        assert!(m.ends_with("outlet_2)"));
    }

    #[test]
    fn aligns_columns() {
        let t = table(
            &["DEVICE", "POWER"],
            &[
                vec!["psu".into(), "on".into()],
                vec!["soldering".into(), "off".into()],
            ],
        );
        assert_eq!(t, "DEVICE     POWER\npsu        on\nsoldering  off\n");
    }
}

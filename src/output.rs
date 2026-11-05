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
}

impl Report {
    pub fn new(data: &impl Serialize, human: String) -> Self {
        Report {
            data: serde_json::to_value(data).unwrap_or(Value::Null),
            human,
            failure: None,
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
            let code = report.failure.as_ref().map_or(0, |(_, c)| *c);
            if json_mode {
                let mut env = json!({ "ok": report.failure.is_none(), "data": report.data });
                if let Some((err, _)) = &report.failure {
                    env["error"] = json!(err);
                }
                print_json(&env);
            } else {
                print!("{}", report.human);
                if let Some((err, _)) = &report.failure {
                    eprintln!("error: {}", err.message);
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

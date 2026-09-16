//! DuckDB CLI output compression.
//!
//! DuckDB renders results as box-drawing tables where every border character is
//! 3 bytes of UTF-8 and every cell is space-padded to the column width, so the
//! framing costs far more than the data. Strip the borders and padding and emit
//! tab-separated rows, leaving non-table output (errors, `-csv`, `-line`) alone.

use crate::core::runner::{self, RunOptions};
use crate::core::truncate::CAP_LIST;
use crate::core::utils::{resolved_command, strip_ansi};
use anyhow::Result;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("duckdb");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: duckdb {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "duckdb",
        &args.join(" "),
        filter_duckdb_output,
        RunOptions::stdout_only()
            .tee("duckdb")
            .early_exit_on_failure(),
    )
}

const MAX_TABLE_ROWS: usize = CAP_LIST;

fn filter_duckdb_output(output: &str) -> String {
    let clean = strip_ansi(output);
    if clean.trim().is_empty() {
        return String::new();
    }
    // Only box tables are worth rewriting. `-csv`, `-line`, `-json` and error
    // text are already compact, and an error must reach the caller byte-exact,
    // so those pass through the original string untouched.
    if !clean.contains('\u{2502}') {
        return output.to_string();
    }
    filter_table(&clean)
}

/// A rule line (`----`, `--+--`, `==+==`) carries no data, only framing.
/// Requiring a horizontal run keeps a bare `\u{2502}` column line out.
fn is_border(line: &str) -> bool {
    line.chars().any(|c| matches!(c, '\u{2500}' | '\u{2550}'))
        && line.chars().all(|c| {
            matches!(
                c,
                '\u{2500}'
                    | '\u{2502}'
                    | '\u{250c}'
                    | '\u{2510}'
                    | '\u{2514}'
                    | '\u{2518}'
                    | '\u{251c}'
                    | '\u{2524}'
                    | '\u{252c}'
                    | '\u{2534}'
                    | '\u{253c}'
                    | '\u{2550}'
                    | '\u{255e}'
                    | '\u{2561}'
                    | '\u{256a}'
            )
        })
}

fn push_overflow(result: &mut Vec<String>, data_rows: usize) {
    if data_rows > MAX_TABLE_ROWS {
        result.push(format!("... +{} more rows", data_rows - MAX_TABLE_ROWS));
    }
}

/// Drop borders and cell padding, emit tab-separated rows. duckdb puts the
/// column names and their types above the mid rule and the data below it, so
/// the mid rule is what separates header from body — counting rows from the
/// top would bill the type row as data and skew the truncation marker.
fn filter_table(output: &str) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut data_rows = 0usize;
    let mut in_header = true;

    for line in output.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("-- Loading resources from") {
            continue;
        }
        if is_border(t) {
            if t.starts_with('\u{250c}') {
                // A new table begins; close out the previous one.
                push_overflow(&mut result, data_rows);
                data_rows = 0;
                in_header = true;
            } else if t.starts_with('\u{251c}') || t.starts_with('\u{255e}') {
                in_header = false;
            }
            continue;
        }
        if t.starts_with('\u{2502}') {
            let row = t
                .trim_matches('\u{2502}')
                .split('\u{2502}')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\t");
            if in_header {
                result.push(row);
            } else {
                data_rows += 1;
                if data_rows <= MAX_TABLE_ROWS {
                    result.push(row);
                }
            }
            continue;
        }
        // Footers such as `1938 rows` state the true total, which matters once
        // the body has been truncated.
        result.push(t.to_string());
    }
    push_overflow(&mut result, data_rows);
    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    const REAL_TABLE: &str = include_str!("../../../tests/fixtures/duckdb_table_raw.txt");

    #[test]
    fn test_duckdb_table_token_savings() {
        let out = filter_duckdb_output(REAL_TABLE);
        let savings = 100.0 - (count_tokens(&out) as f64 / count_tokens(REAL_TABLE) as f64 * 100.0);
        assert!(savings >= 60.0, "expected >=60% savings, got {savings:.1}%");
    }

    #[test]
    fn test_duckdb_strips_borders_and_padding() {
        let out = filter_duckdb_output(REAL_TABLE);
        assert!(
            !out.contains('\u{2502}') && !out.contains('\u{2500}'),
            "box-drawing characters survived"
        );
        assert!(
            out.contains("database_name\tschema_name"),
            "header not tab-joined: {}",
            out.lines().next().unwrap_or("")
        );
        assert!(
            !out.contains("Loading resources from"),
            "duckdbrc noise survived"
        );
    }

    #[test]
    fn test_duckdb_truncates_long_tables() {
        let mut t = String::from(
            "\u{250c}\u{2500}\u{2510}\n\u{2502} id \u{2502}\n\u{2502} int64 \u{2502}\n\u{251c}\u{2500}\u{2524}\n",
        );
        for i in 0..50 {
            t.push_str(&format!("\u{2502} {i} \u{2502}\n"));
        }
        t.push_str("\u{2514}\u{2500}\u{2518}\n50 rows\n");
        let out = filter_duckdb_output(&t);
        assert!(out.contains("+30 more rows"), "no truncation marker: {out}");
        assert!(out.contains("50 rows"), "row-count footer dropped");
    }

    #[test]
    fn test_duckdb_passthrough_non_table() {
        let err =
            "Error: Parser Error: syntax error at or near \"SELCT\"\nLINE 1: SELCT 1;\n        ^";
        assert_eq!(filter_duckdb_output(err), err);
    }

    #[test]
    fn test_duckdb_empty() {
        assert_eq!(filter_duckdb_output("   \n"), "");
    }
}

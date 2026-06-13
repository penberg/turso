// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Parsing and execution of the conformance test format.
//!
//! The format is a line-oriented, sqllogictest-inspired DSL. A file is a
//! sequence of records separated by blank lines:
//!
//! ```text
//! # Comments start with '#'.
//!
//! statement ok
//! CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(50))
//!
//! statement error
//! CREATE TABLE                 -- malformed; must be rejected
//!
//! query
//! SELECT id, name FROM t ORDER BY id
//! ----
//! 1<TAB>alice
//! 2<TAB>bob
//! ```
//!
//! (In real files the columns are separated by a single literal tab; `<TAB>`
//! above stands in for one to keep this doc comment tab-free.)
//!
//! * `statement ok` — the SQL must execute without error.
//! * `statement error` — the SQL must fail.
//! * `query` — the SQL must succeed and produce exactly the rows after `----`,
//!   in order, with columns separated by a single tab and SQL `NULL` rendered
//!   as the literal `NULL`.
//!
//! Execution happens entirely over the MySQL wire protocol via the `mysql`
//! client crate, so the same file runs against the Turso MySQL front-end or a
//! real `mysqld`.

use anyhow::{bail, Context, Result};
use mysql::prelude::Queryable;
use mysql::{Conn, Value};

/// One record (test directive plus its payload) parsed from a `.test` file.
#[derive(Debug, Clone)]
pub enum Record {
    /// A statement that must succeed (`ok`) or fail (`!ok`).
    Statement {
        line: usize,
        expect_ok: bool,
        sql: String,
    },
    /// A query whose rows must match `expected` exactly and in order.
    Query {
        line: usize,
        sql: String,
        expected: Vec<String>,
    },
}

/// Parses the text of a `.test` file into records.
pub fn parse(content: &str) -> Result<Vec<Record>> {
    let lines: Vec<&str> = content.lines().collect();
    let mut records = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        let directive_line = i + 1;
        let directive = trimmed.to_string();
        i += 1;

        match directive.as_str() {
            "query" => {
                let mut sql = Vec::new();
                while i < lines.len() && lines[i].trim_end() != "----" {
                    if lines[i].trim().is_empty() {
                        bail!(
                            "line {directive_line}: `query` block is missing its `----` separator"
                        );
                    }
                    sql.push(lines[i]);
                    i += 1;
                }
                if i >= lines.len() {
                    bail!("line {directive_line}: `query` block is missing its `----` separator");
                }
                i += 1; // consume the `----` line
                let mut expected = Vec::new();
                while i < lines.len() && !lines[i].trim().is_empty() {
                    expected.push(lines[i].to_string());
                    i += 1;
                }
                records.push(Record::Query {
                    line: directive_line,
                    sql: sql.join("\n"),
                    expected,
                });
            }
            "statement ok" | "statement error" => {
                let expect_ok = directive.ends_with("ok");
                let mut sql = Vec::new();
                while i < lines.len() && !lines[i].trim().is_empty() {
                    sql.push(lines[i]);
                    i += 1;
                }
                if sql.is_empty() {
                    bail!("line {directive_line}: `{directive}` has no SQL");
                }
                records.push(Record::Statement {
                    line: directive_line,
                    expect_ok,
                    sql: sql.join("\n"),
                });
            }
            other => bail!("line {directive_line}: unknown directive `{other}`"),
        }
    }
    Ok(records)
}

/// The outcome of a single record.
pub enum Outcome {
    Pass,
    Fail { line: usize, message: String },
}

/// Runs all records against an open connection, returning one outcome per record.
pub fn run(conn: &mut Conn, records: &[Record]) -> Vec<Outcome> {
    records.iter().map(|r| run_record(conn, r)).collect()
}

fn run_record(conn: &mut Conn, record: &Record) -> Outcome {
    match record {
        Record::Statement {
            line,
            expect_ok,
            sql,
        } => match (expect_ok, conn.query_drop(sql)) {
            (true, Ok(())) => Outcome::Pass,
            (true, Err(e)) => Outcome::Fail {
                line: *line,
                message: format!("expected success, but statement failed: {e}"),
            },
            (false, Ok(())) => Outcome::Fail {
                line: *line,
                message: "expected an error, but statement succeeded".to_string(),
            },
            (false, Err(_)) => Outcome::Pass,
        },
        Record::Query {
            line,
            sql,
            expected,
        } => match query_rows(conn, sql) {
            Ok(actual) if &actual == expected => Outcome::Pass,
            Ok(actual) => Outcome::Fail {
                line: *line,
                message: format!("result mismatch:\n{}", diff(expected, &actual)),
            },
            Err(e) => Outcome::Fail {
                line: *line,
                message: format!("query failed: {e}"),
            },
        },
    }
}

/// Runs a query and renders each row as tab-separated column text.
fn query_rows(conn: &mut Conn, sql: &str) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    let result = conn.query_iter(sql).context("query_iter")?;
    for row in result {
        let row = row.context("reading row")?;
        let cells: Vec<String> = row.unwrap().iter().map(render_value).collect();
        rows.push(cells.join("\t"));
    }
    Ok(rows)
}

/// Renders a single MySQL value the way the test format expects.
fn render_value(value: &Value) -> String {
    match value {
        Value::NULL => "NULL".to_string(),
        Value::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Date(y, mo, d, h, mi, s, us) => {
            if *us == 0 {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
            } else {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
            }
        }
        Value::Time(neg, days, h, mi, s, us) => {
            let sign = if *neg { "-" } else { "" };
            let hours = *days * 24 + *h as u32;
            if *us == 0 {
                format!("{sign}{hours:02}:{mi:02}:{s:02}")
            } else {
                format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}")
            }
        }
    }
}

/// Renders a compact expected-vs-actual diff for a failed query.
fn diff(expected: &[String], actual: &[String]) -> String {
    let mut out = String::new();
    let n = expected.len().max(actual.len());
    for i in 0..n {
        match (expected.get(i), actual.get(i)) {
            (Some(e), Some(a)) if e == a => out.push_str(&format!("  {e}\n")),
            (Some(e), Some(a)) => out.push_str(&format!("- {e}\n+ {a}\n")),
            (Some(e), None) => out.push_str(&format!("- {e}\n")),
            (None, Some(a)) => out.push_str(&format!("+ {a}\n")),
            (None, None) => {}
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_statements_and_queries() {
        let content = "\
# a comment
statement ok
CREATE TABLE t (id INT)

statement error
CREATE TABLE

query
SELECT id FROM t ORDER BY id
----
1
2
";
        let records = parse(content).unwrap();
        assert_eq!(records.len(), 3);
        assert!(matches!(
            &records[0],
            Record::Statement {
                expect_ok: true,
                ..
            }
        ));
        assert!(matches!(
            &records[1],
            Record::Statement {
                expect_ok: false,
                ..
            }
        ));
        match &records[2] {
            Record::Query { expected, .. } => assert_eq!(expected, &["1", "2"]),
            _ => panic!("expected a query record"),
        }
    }

    #[test]
    fn query_without_separator_is_an_error() {
        let content = "query\nSELECT 1\n";
        assert!(parse(content).is_err());
    }
}

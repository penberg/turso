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
//! * `statement error <code>` — the SQL must fail with that MySQL error number
//!   (e.g. `statement error 1062`).
//! * `query` — the SQL must succeed and produce exactly the rows after `----`,
//!   in order, with columns separated by a single tab and SQL `NULL` rendered
//!   as the literal `NULL`.
//! * `query types` — like `query`, but the single expected line lists the
//!   MySQL column *type* of each result column (e.g. `LONG`, `VAR_STRING`),
//!   checking the result-set metadata rather than the rows.
//! * `query labels` — like `query types`, but the single expected line lists
//!   the column *label* (name) of each result column, checking the default
//!   column naming.
//! * `exec ok` / `exec error` — like `statement`, but run over the binary
//!   protocol as a prepared statement. An optional `params` line binds the
//!   `?` placeholders (tab-separated; `NULL` for SQL NULL).
//! * `exec query` — like `query`, but run as a prepared statement with an
//!   optional `params` line before the `----` separator.
//!
//! Execution happens entirely over the MySQL wire protocol via the `mysql`
//! client crate, so the same file runs against the Turso MySQL front-end or a
//! real `mysqld`. The `exec*` directives exercise the binary (prepared
//! statement) protocol; the others use the text protocol.

use anyhow::{bail, Context, Result};
use mysql::consts::ColumnType;
use mysql::prelude::Queryable;
use mysql::{Conn, Params, Value};

/// One record (test directive plus its payload) parsed from a `.test` file.
#[derive(Debug, Clone)]
pub enum Record {
    /// A statement that must succeed (`ok`) or fail (`!ok`). When `expect_ok` is
    /// false, `expect_code` optionally pins the MySQL error code the failure must
    /// carry (`statement error 1062`).
    Statement {
        line: usize,
        expect_ok: bool,
        expect_code: Option<u16>,
        sql: String,
    },
    /// A query whose rows must match `expected` exactly and in order.
    Query {
        line: usize,
        sql: String,
        expected: Vec<String>,
    },
    /// A query whose result-column types must match `expected` (a single line
    /// of tab-separated MySQL type names).
    QueryTypes {
        line: usize,
        sql: String,
        expected: Vec<String>,
    },
    /// A query whose result-column *labels* (names) must match `expected` (a
    /// single line of tab-separated column names).
    QueryLabels {
        line: usize,
        sql: String,
        expected: Vec<String>,
    },
    /// A prepared statement (binary protocol) that must succeed or fail, with
    /// its `?` placeholders bound from `params`.
    Exec {
        line: usize,
        expect_ok: bool,
        sql: String,
        params: Vec<String>,
    },
    /// A prepared query (binary protocol) whose rows must match `expected`,
    /// with its `?` placeholders bound from `params`.
    ExecQuery {
        line: usize,
        sql: String,
        params: Vec<String>,
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
            "statement ok" => {
                let sql = read_statement_sql(&lines, &mut i, &directive, directive_line)?;
                records.push(Record::Statement {
                    line: directive_line,
                    expect_ok: true,
                    expect_code: None,
                    sql,
                });
            }
            // `statement error` or `statement error <code>` (the optional code
            // pins the MySQL error number, e.g. `statement error 1062`).
            d if d == "statement error" || d.starts_with("statement error ") => {
                let expect_code = match d.strip_prefix("statement error ").map(str::trim) {
                    Some(code) if !code.is_empty() => Some(code.parse::<u16>().with_context(|| {
                        format!("line {directive_line}: invalid error code `{code}`")
                    })?),
                    _ => None,
                };
                let sql = read_statement_sql(&lines, &mut i, &directive, directive_line)?;
                records.push(Record::Statement {
                    line: directive_line,
                    expect_ok: false,
                    expect_code,
                    sql,
                });
            }
            "query types" => {
                let (sql, expected) = read_query_block(&lines, &mut i, directive_line)?;
                records.push(Record::QueryTypes {
                    line: directive_line,
                    sql,
                    expected,
                });
            }
            "query labels" => {
                let (sql, expected) = read_query_block(&lines, &mut i, directive_line)?;
                records.push(Record::QueryLabels {
                    line: directive_line,
                    sql,
                    expected,
                });
            }
            "exec ok" | "exec error" => {
                let expect_ok = directive.ends_with("ok");
                let (sql, params) =
                    read_sql_with_params(&lines, &mut i, &directive, directive_line)?;
                records.push(Record::Exec {
                    line: directive_line,
                    expect_ok,
                    sql,
                    params,
                });
            }
            "exec query" => {
                let (sql, params, expected) = read_exec_query(&lines, &mut i, directive_line)?;
                records.push(Record::ExecQuery {
                    line: directive_line,
                    sql,
                    params,
                    expected,
                });
            }
            other => bail!("line {directive_line}: unknown directive `{other}`"),
        }
    }
    Ok(records)
}

/// Reads the SQL body of a `statement` directive — the lines up to the next
/// blank line. Advances `i` past them.
fn read_statement_sql(
    lines: &[&str],
    i: &mut usize,
    directive: &str,
    directive_line: usize,
) -> Result<String> {
    let mut sql = Vec::new();
    while *i < lines.len() && !lines[*i].trim().is_empty() {
        sql.push(lines[*i]);
        *i += 1;
    }
    if sql.is_empty() {
        bail!("line {directive_line}: `{directive}` has no SQL");
    }
    Ok(sql.join("\n"))
}

/// Reads a query block: the SQL lines up to a `----` separator, then the
/// expected lines up to a blank line. Advances `i` past the block.
fn read_query_block(
    lines: &[&str],
    i: &mut usize,
    directive_line: usize,
) -> Result<(String, Vec<String>)> {
    let mut sql = Vec::new();
    while *i < lines.len() && lines[*i].trim_end() != "----" {
        if lines[*i].trim().is_empty() {
            bail!("line {directive_line}: query block is missing its `----` separator");
        }
        sql.push(lines[*i]);
        *i += 1;
    }
    if *i >= lines.len() {
        bail!("line {directive_line}: query block is missing its `----` separator");
    }
    *i += 1; // consume the `----` line
    let mut expected = Vec::new();
    while *i < lines.len() && !lines[*i].trim().is_empty() {
        expected.push(lines[*i].to_string());
        *i += 1;
    }
    Ok((sql.join("\n"), expected))
}

/// Reads SQL lines up to a blank line or a `params` line. Returns the SQL and
/// any bound parameters.
fn read_sql_with_params(
    lines: &[&str],
    i: &mut usize,
    directive: &str,
    directive_line: usize,
) -> Result<(String, Vec<String>)> {
    let mut sql = Vec::new();
    let mut params = Vec::new();
    while *i < lines.len() && !lines[*i].trim().is_empty() {
        if let Some(values) = parse_params_line(lines[*i]) {
            params = values;
            *i += 1;
            break;
        }
        sql.push(lines[*i]);
        *i += 1;
    }
    if sql.is_empty() {
        bail!("line {directive_line}: `{directive}` has no SQL");
    }
    Ok((sql.join("\n"), params))
}

/// Reads an `exec query` block: SQL, an optional `params` line, the `----`
/// separator, then the expected rows.
fn read_exec_query(
    lines: &[&str],
    i: &mut usize,
    directive_line: usize,
) -> Result<(String, Vec<String>, Vec<String>)> {
    let mut sql = Vec::new();
    let mut params = Vec::new();
    loop {
        if *i >= lines.len() {
            bail!("line {directive_line}: `exec query` block is missing its `----` separator");
        }
        let line = lines[*i];
        if line.trim_end() == "----" {
            break;
        }
        if line.trim().is_empty() {
            bail!("line {directive_line}: `exec query` block is missing its `----` separator");
        }
        if let Some(values) = parse_params_line(line) {
            params = values;
            *i += 1;
            continue;
        }
        sql.push(line);
        *i += 1;
    }
    *i += 1; // consume the `----` line
    let mut expected = Vec::new();
    while *i < lines.len() && !lines[*i].trim().is_empty() {
        expected.push(lines[*i].to_string());
        *i += 1;
    }
    Ok((sql.join("\n"), params, expected))
}

/// Parses a `params` line into its tab-separated values, or returns `None` if
/// the line is not a `params` line.
fn parse_params_line(line: &str) -> Option<Vec<String>> {
    let rest = line.trim().strip_prefix("params")?;
    // Require a separator after the keyword so `paramsX` is not mistaken for it.
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some(rest.trim_start().split('\t').map(str::to_string).collect())
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

/// The MySQL error number a driver error carries, if any (a server-side error
/// reported over the protocol, as opposed to a client/IO error).
fn mysql_error_code(error: &mysql::Error) -> Option<u16> {
    match error {
        mysql::Error::MySqlError(e) => Some(e.code),
        _ => None,
    }
}

fn run_record(conn: &mut Conn, record: &Record) -> Outcome {
    match record {
        Record::Statement {
            line,
            expect_ok,
            expect_code,
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
            (false, Err(e)) => match expect_code {
                None => Outcome::Pass,
                Some(want) => match mysql_error_code(&e) {
                    Some(got) if got == *want => Outcome::Pass,
                    got => Outcome::Fail {
                        line: *line,
                        message: format!(
                            "expected error code {want}, got {}: {e}",
                            got.map_or_else(|| "none".to_string(), |c| c.to_string())
                        ),
                    },
                },
            },
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
        Record::QueryTypes {
            line,
            sql,
            expected,
        } => match query_types(conn, sql) {
            Ok(actual) if &actual == expected => Outcome::Pass,
            Ok(actual) => Outcome::Fail {
                line: *line,
                message: format!("column-type mismatch:\n{}", diff(expected, &actual)),
            },
            Err(e) => Outcome::Fail {
                line: *line,
                message: format!("query failed: {e}"),
            },
        },
        Record::QueryLabels {
            line,
            sql,
            expected,
        } => match query_labels(conn, sql) {
            Ok(actual) if &actual == expected => Outcome::Pass,
            Ok(actual) => Outcome::Fail {
                line: *line,
                message: format!("column-label mismatch:\n{}", diff(expected, &actual)),
            },
            Err(e) => Outcome::Fail {
                line: *line,
                message: format!("query failed: {e}"),
            },
        },
        Record::Exec {
            line,
            expect_ok,
            sql,
            params,
        } => match (expect_ok, conn.exec_drop(sql, to_params(params))) {
            (true, Ok(())) => Outcome::Pass,
            (true, Err(e)) => Outcome::Fail {
                line: *line,
                message: format!("expected success, but prepared statement failed: {e}"),
            },
            (false, Ok(())) => Outcome::Fail {
                line: *line,
                message: "expected an error, but prepared statement succeeded".to_string(),
            },
            (false, Err(_)) => Outcome::Pass,
        },
        Record::ExecQuery {
            line,
            sql,
            params,
            expected,
        } => match exec_query_rows(conn, sql, to_params(params)) {
            Ok(actual) if &actual == expected => Outcome::Pass,
            Ok(actual) => Outcome::Fail {
                line: *line,
                message: format!("result mismatch:\n{}", diff(expected, &actual)),
            },
            Err(e) => Outcome::Fail {
                line: *line,
                message: format!("prepared query failed: {e}"),
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

/// Runs a query and renders its result-set column types as a single
/// tab-separated line (e.g. `LONG<TAB>VAR_STRING`).
fn query_types(conn: &mut Conn, sql: &str) -> Result<Vec<String>> {
    let result = conn.query_iter(sql).context("query_iter")?;
    let types: Vec<String> = result
        .columns()
        .as_ref()
        .iter()
        .map(|column| type_name(column.column_type()))
        .collect();
    Ok(vec![types.join("\t")])
}

/// Runs a query and renders its result-set column labels (names) as a single
/// tab-separated line.
fn query_labels(conn: &mut Conn, sql: &str) -> Result<Vec<String>> {
    let result = conn.query_iter(sql).context("query_iter")?;
    let labels: Vec<String> = result
        .columns()
        .as_ref()
        .iter()
        .map(|column| column.name_str().into_owned())
        .collect();
    Ok(vec![labels.join("\t")])
}

/// Runs a prepared query over the binary protocol and renders its rows.
fn exec_query_rows(conn: &mut Conn, sql: &str, params: Params) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    let result = conn.exec_iter(sql, params).context("exec_iter")?;
    for row in result {
        let row = row.context("reading row")?;
        let cells: Vec<String> = row.unwrap().iter().map(render_value).collect();
        rows.push(cells.join("\t"));
    }
    Ok(rows)
}

/// Maps a MySQL column type to its short name, dropping the `MYSQL_TYPE_`
/// prefix (`MYSQL_TYPE_LONG` becomes `LONG`).
fn type_name(ty: ColumnType) -> String {
    format!("{ty:?}")
        .strip_prefix("MYSQL_TYPE_")
        .map(str::to_string)
        .unwrap_or_else(|| format!("{ty:?}"))
}

/// Converts parsed parameter tokens into bound values. A token that parses as
/// an integer or float binds as that number; `NULL` binds as SQL NULL;
/// everything else binds as a string.
fn to_params(tokens: &[String]) -> Params {
    if tokens.is_empty() {
        return Params::Empty;
    }
    let values = tokens
        .iter()
        .map(|token| {
            if token == "NULL" {
                Value::NULL
            } else if let Ok(i) = token.parse::<i64>() {
                Value::Int(i)
            } else if let Ok(f) = token.parse::<f64>() {
                Value::Double(f)
            } else {
                Value::Bytes(token.as_bytes().to_vec())
            }
        })
        .collect();
    Params::Positional(values)
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

    #[test]
    fn parses_statement_error_with_code() {
        let content = "\
statement error 1062
INSERT INTO t VALUES (1)

statement error
INSERT INTO t VALUES (2)
";
        let records = parse(content).unwrap();
        assert!(matches!(
            &records[0],
            Record::Statement {
                expect_ok: false,
                expect_code: Some(1062),
                ..
            }
        ));
        // The bare form leaves the code unpinned.
        assert!(matches!(
            &records[1],
            Record::Statement {
                expect_ok: false,
                expect_code: None,
                ..
            }
        ));
    }

    #[test]
    fn statement_error_with_bad_code_is_an_error() {
        assert!(parse("statement error notanumber\nSELECT 1\n").is_err());
    }
}

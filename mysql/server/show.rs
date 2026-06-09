// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! `SHOW [FULL] COLUMNS FROM <table>` support.
//!
//! WordPress probes every table with `SHOW FULL COLUMNS FROM` during install to
//! learn each column's format. The engine's AST has no `SHOW`, so — like the
//! session/introspection queries in [`crate::session`] — these are handled here,
//! ahead of the front-end parser. Unlike the session queries this one needs the
//! schema, so it reads the table's columns from the engine via
//! `PRAGMA table_info` and reshapes them into MySQL's result-set columns.
//!
//! Only the result-set *shape* and the schema-derived columns (`Field`, `Null`,
//! `Key`, `Default`) are reproduced faithfully. MySQL's type-display
//! normalization (column sizes, `unsigned`, integer display-width stripping) and
//! the exact `Collation`/`Extra` text are not modeled — see `mysql/COMPAT.md`.

use std::sync::Arc;

use turso_core::{Connection, LimboError, Value};

/// `Privileges` is a constant in MySQL's `SHOW FULL COLUMNS` output.
const PRIVILEGES: &str = "select,insert,update,references";
/// Collation reported for text columns (non-text columns report SQL `NULL`).
/// Matches the value the rest of the front-end advertises in [`crate::session`].
const COLLATION: &str = "utf8mb4_general_ci";

/// A synthesized result set: column headers plus text rows (`None` = SQL `NULL`).
pub struct ColumnsResult {
    pub columns: Vec<&'static str>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// The outcome of handling a `SHOW [FULL] COLUMNS` statement.
pub enum ShowOutcome {
    /// A built result set.
    Columns(ColumnsResult),
    /// The statement named a table that does not exist (MySQL error 1146).
    NoSuchTable(String),
}

/// Tries to handle `sql` as `SHOW [FULL] {COLUMNS|FIELDS} {FROM|IN} tbl` or
/// `SHOW [FULL] TABLES [LIKE 'pat']`.
///
/// Returns `None` if `sql` is neither, so every other `SHOW` form falls through
/// to the parser (which rejects it as unsupported).
pub fn try_handle(conn: &Arc<Connection>, sql: &str) -> Option<Result<ShowOutcome, LimboError>> {
    if let Some(parsed) = parse_show_columns(sql) {
        return Some(build(conn, &parsed));
    }
    if let Some(parsed) = parse_show_tables(sql) {
        return Some(build_tables(conn, &parsed).map(ShowOutcome::Columns));
    }
    None
}

/// The parsed form of a `SHOW [FULL] COLUMNS FROM tbl` statement.
struct ShowColumns {
    full: bool,
    table: String,
}

/// Reads the table's columns via `PRAGMA table_info` and reshapes them into the
/// MySQL `SHOW [FULL] COLUMNS` result set.
fn build(conn: &Arc<Connection>, show: &ShowColumns) -> Result<ShowOutcome, LimboError> {
    let pragma = format!("PRAGMA table_info('{}')", show.table.replace('\'', "''"));
    let Some(mut stmt) = conn.query(&pragma)? else {
        return Ok(ShowOutcome::NoSuchTable(show.table.clone()));
    };

    let mut info: Vec<ColumnInfo> = Vec::new();
    stmt.run_with_row_callback(|row| {
        // PRAGMA table_info columns: cid, name, type, notnull, dflt_value, pk.
        let name = value_to_string(row.get_value(1)).unwrap_or_default();
        let ty = value_to_string(row.get_value(2)).unwrap_or_default();
        let notnull = value_to_string(row.get_value(3)).as_deref() != Some("0");
        let default = value_to_string(row.get_value(4));
        let pk = value_to_string(row.get_value(5)).as_deref() != Some("0");
        info.push(ColumnInfo {
            name,
            ty,
            notnull,
            default,
            pk,
        });
        Ok(())
    })?;

    // An existing table always has at least one column, so an empty result means
    // the table does not exist.
    if info.is_empty() {
        return Ok(ShowOutcome::NoSuchTable(show.table.clone()));
    }

    let columns = if show.full {
        vec![
            "Field",
            "Type",
            "Collation",
            "Null",
            "Key",
            "Default",
            "Extra",
            "Privileges",
            "Comment",
        ]
    } else {
        vec!["Field", "Type", "Null", "Key", "Default", "Extra"]
    };

    let rows = info
        .into_iter()
        .map(|c| c.into_row(show.full))
        .collect::<Vec<_>>();

    Ok(ShowOutcome::Columns(ColumnsResult { columns, rows }))
}

/// The parsed form of a `SHOW [FULL] TABLES [LIKE 'pat']` statement.
struct ShowTables {
    full: bool,
    like: Option<String>,
}

/// Lists base table names from the schema, optionally filtered by a `LIKE`
/// pattern, as a MySQL `SHOW [FULL] TABLES` result set.
fn build_tables(conn: &Arc<Connection>, show: &ShowTables) -> Result<ColumnsResult, LimboError> {
    let mut query =
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"
            .to_string();
    if let Some(pat) = &show.like {
        query.push_str(&format!(" AND name LIKE '{}'", pat.replace('\'', "''")));
    }
    query.push_str(" ORDER BY name");

    let mut names: Vec<String> = Vec::new();
    if let Some(mut stmt) = conn.query(&query)? {
        stmt.run_with_row_callback(|row| {
            names.push(value_to_string(row.get_value(0)).unwrap_or_default());
            Ok(())
        })?;
    }

    // MySQL's header is `Tables_in_<db>`; the front-end ignores schema selection,
    // and clients read this column positionally, so a fixed header is used.
    let columns = if show.full {
        vec!["Tables_in_database", "Table_type"]
    } else {
        vec!["Tables_in_database"]
    };
    let rows = names
        .into_iter()
        .map(|name| {
            if show.full {
                vec![Some(name), Some("BASE TABLE".to_string())]
            } else {
                vec![Some(name)]
            }
        })
        .collect();

    Ok(ColumnsResult { columns, rows })
}

/// One column as read from `PRAGMA table_info`.
struct ColumnInfo {
    name: String,
    ty: String,
    notnull: bool,
    default: Option<String>,
    pk: bool,
}

impl ColumnInfo {
    /// Reshapes the column into a MySQL `SHOW [FULL] COLUMNS` row.
    fn into_row(self, full: bool) -> Vec<Option<String>> {
        let null = if self.notnull { "NO" } else { "YES" };
        let key = if self.pk { "PRI" } else { "" };
        let collation = if is_text_type(&self.ty) {
            Some(COLLATION.to_string())
        } else {
            None
        };
        // MySQL lowercases the type name in this output.
        let ty = Some(self.ty.to_ascii_lowercase());
        let field = Some(self.name);
        if full {
            vec![
                field,
                ty,
                collation,
                Some(null.to_string()),
                Some(key.to_string()),
                self.default,
                Some(String::new()), // Extra
                Some(PRIVILEGES.to_string()),
                Some(String::new()), // Comment
            ]
        } else {
            vec![
                field,
                ty,
                Some(null.to_string()),
                Some(key.to_string()),
                self.default,
                Some(String::new()), // Extra
            ]
        }
    }
}

/// Whether a declared type is character data (and so carries a collation).
fn is_text_type(ty: &str) -> bool {
    let upper = ty.to_ascii_uppercase();
    upper.contains("CHAR") || upper.contains("TEXT") || upper.contains("CLOB")
}

/// Renders a value as text, or `None` for SQL `NULL`.
fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(t) => Some(t.as_str().to_string()),
        Value::Blob(b) => Some(String::from_utf8_lossy(b).to_string()),
        other => Some(format!("{other}")),
    }
}

/// Parses `SHOW [FULL] {COLUMNS|FIELDS} {FROM|IN} tbl [{FROM|IN} db]`. Returns
/// `None` for any other statement, including `SHOW COLUMNS ... LIKE`/`WHERE`
/// (not yet handled) so those fall through to the parser.
fn parse_show_columns(sql: &str) -> Option<ShowColumns> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let toks = tokenize(trimmed);
    let mut k = 0;

    let kw = |t: &str, kw: &str| t.eq_ignore_ascii_case(kw);

    if !toks.get(k).is_some_and(|t| kw(t, "SHOW")) {
        return None;
    }
    k += 1;
    let full = toks.get(k).is_some_and(|t| kw(t, "FULL"));
    if full {
        k += 1;
    }
    if !toks
        .get(k)
        .is_some_and(|t| kw(t, "COLUMNS") || kw(t, "FIELDS"))
    {
        return None;
    }
    k += 1;
    if !toks.get(k).is_some_and(|t| kw(t, "FROM") || kw(t, "IN")) {
        return None;
    }
    k += 1;

    let mut table = toks.get(k)?.clone();
    k += 1;
    // `db.tbl`: the real table name follows the dot.
    if toks.get(k).is_some_and(|t| t == ".") {
        k += 1;
        table = toks.get(k)?.clone();
        k += 1;
    }
    // Optional `{FROM|IN} db` qualifier; consumed and ignored.
    if toks.get(k).is_some_and(|t| kw(t, "FROM") || kw(t, "IN")) {
        k += 1;
        toks.get(k)?;
        k += 1;
    }

    // Any trailing tokens (e.g. LIKE/WHERE) are not handled here.
    if k != toks.len() {
        return None;
    }
    Some(ShowColumns { full, table })
}

/// Parses `SHOW [FULL] TABLES [{FROM|IN} db] [LIKE 'pat']`. Returns `None` for
/// any other statement, including `SHOW TABLES ... WHERE ...` (not handled).
fn parse_show_tables(sql: &str) -> Option<ShowTables> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let toks = tokenize(trimmed);
    let mut k = 0;

    let kw = |t: &str, kw: &str| t.eq_ignore_ascii_case(kw);

    if !toks.get(k).is_some_and(|t| kw(t, "SHOW")) {
        return None;
    }
    k += 1;
    let full = toks.get(k).is_some_and(|t| kw(t, "FULL"));
    if full {
        k += 1;
    }
    if !toks.get(k).is_some_and(|t| kw(t, "TABLES")) {
        return None;
    }
    k += 1;

    // Optional `{FROM|IN} db` qualifier; consumed and ignored.
    if toks.get(k).is_some_and(|t| kw(t, "FROM") || kw(t, "IN")) {
        k += 1;
        toks.get(k)?;
        k += 1;
    }

    // Optional `LIKE 'pattern'`.
    let like = if toks.get(k).is_some_and(|t| kw(t, "LIKE")) {
        k += 1;
        let pat = toks.get(k)?;
        // The pattern is a quoted string token; strip its surrounding quotes.
        let unquoted = pat
            .strip_prefix('\'')
            .and_then(|p| p.strip_suffix('\''))
            .or_else(|| pat.strip_prefix('"').and_then(|p| p.strip_suffix('"')))?;
        k += 1;
        Some(unquoted.to_string())
    } else {
        None
    };

    // Any trailing tokens (e.g. WHERE) are not handled here.
    if k != toks.len() {
        return None;
    }
    Some(ShowTables { full, like })
}

/// Splits a statement into tokens, unquoting backtick identifiers and keeping
/// `.` as its own token. Quoted strings are kept verbatim (quotes included).
fn tokenize(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '`' {
            chars.next();
            let mut name = String::new();
            while let Some(ch) = chars.next() {
                if ch == '`' {
                    if chars.peek() == Some(&'`') {
                        chars.next();
                        name.push('`');
                    } else {
                        break;
                    }
                } else {
                    name.push(ch);
                }
            }
            toks.push(name);
        } else if c == '\'' || c == '"' {
            let quote = c;
            let mut s2 = String::new();
            s2.push(quote);
            chars.next();
            for ch in chars.by_ref() {
                s2.push(ch);
                if ch == quote {
                    break;
                }
            }
            toks.push(s2);
        } else if matches!(c, '.' | ',' | ';' | '(' | ')') {
            chars.next();
            toks.push(c.to_string());
        } else {
            let mut w = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace()
                    || matches!(ch, '`' | '\'' | '"' | '.' | ',' | ';' | '(' | ')')
                {
                    break;
                }
                w.push(ch);
                chars.next();
            }
            toks.push(w);
        }
    }
    toks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_columns_from_backtick_table() {
        let p = parse_show_columns("SHOW FULL COLUMNS FROM `wptests_options`").unwrap();
        assert!(p.full);
        assert_eq!(p.table, "wptests_options");
    }

    #[test]
    fn parses_show_tables() {
        let p = parse_show_tables("SHOW TABLES").unwrap();
        assert!(!p.full);
        assert_eq!(p.like, None);

        let p = parse_show_tables("SHOW FULL TABLES LIKE 'wp_%'").unwrap();
        assert!(p.full);
        assert_eq!(p.like.as_deref(), Some("wp_%"));

        // `FROM db` qualifier is accepted and ignored.
        assert!(parse_show_tables("SHOW TABLES FROM mydb LIKE 'a%'").is_some());

        // Not SHOW TABLES, or an unhandled WHERE form.
        assert!(parse_show_tables("SHOW COLUMNS FROM t").is_none());
        assert!(parse_show_tables("SHOW TABLES WHERE 1").is_none());
    }

    #[test]
    fn parses_plain_columns_and_fields_synonym() {
        let p = parse_show_columns("SHOW COLUMNS FROM t").unwrap();
        assert!(!p.full);
        assert_eq!(p.table, "t");
        assert_eq!(parse_show_columns("SHOW FIELDS IN t").unwrap().table, "t");
    }

    #[test]
    fn parses_db_qualified_table() {
        let p = parse_show_columns("SHOW COLUMNS FROM `mydb`.`t`").unwrap();
        assert_eq!(p.table, "t");
    }

    #[test]
    fn rejects_unrelated_and_unhandled_forms() {
        assert!(parse_show_columns("SHOW TABLES").is_none());
        assert!(parse_show_columns("SHOW VARIABLES LIKE 'x'").is_none());
        assert!(parse_show_columns("SELECT 1").is_none());
        // LIKE/WHERE filters are not handled yet and fall through to the parser.
        assert!(parse_show_columns("SHOW COLUMNS FROM t LIKE 'a%'").is_none());
    }

    #[test]
    fn text_types_carry_a_collation() {
        assert!(is_text_type("VARCHAR"));
        assert!(is_text_type("text"));
        assert!(is_text_type("LONGTEXT"));
        assert!(!is_text_type("INT"));
        assert!(!is_text_type("BIGINT"));
    }
}

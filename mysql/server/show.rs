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
//! The result-set *shape*, the schema-derived columns (`Field`, `Null`, `Key`,
//! `Default`), and the declared column size (`varchar(60)`, recovered from the
//! stored `CREATE TABLE` text since `PRAGMA table_info` drops it) are reproduced
//! faithfully. MySQL's other type-display normalization (`unsigned`, integer
//! display-width stripping) and the exact `Collation`/`Extra` text are not
//! modeled — see `mysql/COMPAT.md`.

use std::collections::HashMap;
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

/// Tries to handle `sql` as `SHOW [FULL] {COLUMNS|FIELDS} {FROM|IN} tbl`,
/// `{DESCRIBE|DESC} tbl` (a synonym for the non-FULL form),
/// `SHOW [FULL] TABLES [LIKE 'pat']`, or
/// `SHOW {INDEX|INDEXES|KEYS} {FROM|IN} tbl`.
///
/// Returns `None` if `sql` is none of these, so every other `SHOW` form falls
/// through to the parser (which rejects it as unsupported).
pub fn try_handle(conn: &Arc<Connection>, sql: &str) -> Option<Result<ShowOutcome, LimboError>> {
    if let Some(parsed) = parse_show_columns(sql).or_else(|| parse_describe(sql)) {
        return Some(build(conn, &parsed));
    }
    if let Some(parsed) = parse_show_tables(sql) {
        return Some(build_tables(conn, &parsed).map(ShowOutcome::Columns));
    }
    if let Some(parsed) = parse_show_index(sql) {
        return Some(build_index(conn, &parsed));
    }
    if let Some(parsed) = parse_show_table_status(sql) {
        return Some(build_table_status(conn, &parsed).map(ShowOutcome::Columns));
    }
    if let Some(parsed) = parse_show_variables(sql) {
        return Some(Ok(ShowOutcome::Columns(build_variables(&parsed))));
    }
    if let Some(result) = parse_show_warnings(sql) {
        return Some(Ok(ShowOutcome::Columns(result)));
    }
    if let Some(result) = parse_show_empty_enumeration(sql) {
        return Some(Ok(ShowOutcome::Columns(result)));
    }
    if let Some(result) = parse_maintenance(sql) {
        return Some(Ok(ShowOutcome::Columns(result)));
    }
    None
}

/// Handles the object-enumeration statements `SHOW TRIGGERS`, `SHOW EVENTS`,
/// `SHOW PROCEDURE STATUS`, and `SHOW FUNCTION STATUS`. This engine has no
/// triggers, scheduled events, or stored routines, so each is always empty — the
/// result is MySQL's column set with no rows, which matches a real mysqld on a
/// schema that has none of those objects. WordPress backup / migration plugins
/// enumerate these while exporting a database, so answering with an empty set (as
/// opposed to an error) lets that flow proceed. Any trailing `FROM db` /
/// `LIKE 'pat'` / `WHERE ...` filter is accepted and irrelevant on an empty set.
/// Returns `None` for any other statement (notably plain `SHOW STATUS`, whose
/// runtime counters are not modeled).
fn parse_show_empty_enumeration(sql: &str) -> Option<ColumnsResult> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let toks = tokenize(trimmed);
    let kw = |i: usize, k: &str| toks.get(i).is_some_and(|t| t.eq_ignore_ascii_case(k));
    if !kw(0, "SHOW") {
        return None;
    }
    let columns = if kw(1, "TRIGGERS") {
        vec![
            "Trigger",
            "Event",
            "Table",
            "Statement",
            "Timing",
            "Created",
            "sql_mode",
            "Definer",
            "character_set_client",
            "collation_connection",
            "Database Collation",
        ]
    } else if kw(1, "EVENTS") {
        vec![
            "Db",
            "Name",
            "Definer",
            "Time zone",
            "Type",
            "Execute at",
            "Interval value",
            "Interval field",
            "Starts",
            "Ends",
            "Status",
            "Originator",
            "character_set_client",
            "collation_connection",
            "Database Collation",
        ]
    } else if (kw(1, "PROCEDURE") || kw(1, "FUNCTION")) && kw(2, "STATUS") {
        vec![
            "Db",
            "Name",
            "Type",
            "Language",
            "Definer",
            "Modified",
            "Created",
            "Security_type",
            "Comment",
            "character_set_client",
            "collation_connection",
            "Database Collation",
        ]
    } else {
        return None;
    };
    Some(ColumnsResult {
        columns,
        rows: Vec::new(),
    })
}

/// Handles the table-maintenance statements `{ANALYZE | CHECK | OPTIMIZE |
/// REPAIR} TABLE tbl [, tbl] ...` (WordPress's database-repair admin page runs
/// these). The engine has no fragmentation, optimizer statistics, or
/// MySQL-style corruption, so each is a no-op that reports success: the result
/// is MySQL's `Table` / `Op` / `Msg_type` / `Msg_text` columns with one
/// `status` / `OK` row per named table. The `Table` value is the bare table name
/// (the engine has no schema-qualified name, so it is not `db.tbl` as MySQL
/// reports), and trailing options (`QUICK`, `EXTENDED`, `FOR UPGRADE`, …) are
/// ignored. Returns `None` for any other statement.
fn parse_maintenance(sql: &str) -> Option<ColumnsResult> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let toks = tokenize(trimmed);
    let kw = |t: &str, k: &str| t.eq_ignore_ascii_case(k);

    let op = match toks.first()? {
        t if kw(t, "ANALYZE") => "analyze",
        t if kw(t, "CHECK") => "check",
        t if kw(t, "OPTIMIZE") => "optimize",
        t if kw(t, "REPAIR") => "repair",
        _ => return None,
    };
    let mut k = 1;
    // An optional `NO_WRITE_TO_BINLOG` / `LOCAL` modifier precedes `TABLE`.
    if toks
        .get(k)
        .is_some_and(|t| kw(t, "NO_WRITE_TO_BINLOG") || kw(t, "LOCAL"))
    {
        k += 1;
    }
    if !toks.get(k).is_some_and(|t| kw(t, "TABLE")) {
        return None;
    }
    k += 1;

    // The comma-separated table list (each possibly `db.tbl`). The list ends at
    // the first token that is not followed by a comma; any trailing option
    // keywords are left unconsumed and ignored.
    let mut tables = Vec::new();
    loop {
        let Some(first) = toks.get(k) else { break };
        let mut table = first.clone();
        k += 1;
        if toks.get(k).is_some_and(|t| t == ".") {
            k += 1;
            match toks.get(k) {
                Some(t) => {
                    table = t.clone();
                    k += 1;
                }
                None => return None,
            }
        }
        tables.push(table);
        if toks.get(k).is_some_and(|t| t == ",") {
            k += 1;
            continue;
        }
        break;
    }
    if tables.is_empty() {
        return None;
    }

    let rows = tables
        .into_iter()
        .map(|t| {
            vec![
                Some(t),
                Some(op.to_string()),
                Some("status".to_string()),
                Some("OK".to_string()),
            ]
        })
        .collect();
    Some(ColumnsResult {
        columns: vec!["Table", "Op", "Msg_type", "Msg_text"],
        rows,
    })
}

/// Handles `SHOW {WARNINGS | ERRORS} [LIMIT ...]`. The engine raises no
/// persistent warnings or errors, so the diagnostics area is always empty: the
/// result is MySQL's `Level` / `Code` / `Message` columns with no rows, which
/// matches a real mysqld after a statement that produced no warnings. Clients
/// (e.g. `mysqli` with warning reporting on) issue this after each statement. A
/// trailing `LIMIT` is accepted and irrelevant on an empty set. Returns `None`
/// for any other statement.
fn parse_show_warnings(sql: &str) -> Option<ColumnsResult> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let toks = tokenize(trimmed);
    let kw = |i: usize, kw: &str| toks.get(i).is_some_and(|t| t.eq_ignore_ascii_case(kw));
    if !kw(0, "SHOW") || !(kw(1, "WARNINGS") || kw(1, "ERRORS")) {
        return None;
    }
    // Only an optional `LIMIT ...` may follow; reject the `COUNT(*)` form and
    // anything else so it falls through.
    if toks.len() > 2 && !kw(2, "LIMIT") {
        return None;
    }
    Some(ColumnsResult {
        columns: vec!["Level", "Code", "Message"],
        rows: Vec::new(),
    })
}

/// The parsed form of a `SHOW [FULL] COLUMNS FROM tbl` statement.
struct ShowColumns {
    full: bool,
    table: String,
    filter: Option<ColumnFilter>,
}

/// A single-predicate filter on the `SHOW COLUMNS` output, from a trailing
/// `LIKE 'pat'` (matched against `Field`) or `WHERE col {= | LIKE} value`.
struct ColumnFilter {
    /// The output column matched against (one of [`COLUMNS_FULL`] /
    /// [`COLUMNS_BASE`]); `Field` for the `LIKE 'pat'` form.
    column: String,
    like: bool,
    value: String,
}

/// The columns of `SHOW FULL COLUMNS`, in order.
const COLUMNS_FULL: [&str; 9] = [
    "Field",
    "Type",
    "Collation",
    "Null",
    "Key",
    "Default",
    "Extra",
    "Privileges",
    "Comment",
];

/// The columns of the non-FULL `SHOW COLUMNS`, in order.
const COLUMNS_BASE: [&str; 6] = ["Field", "Type", "Null", "Key", "Default", "Extra"];

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
            key: "",
        });
        Ok(())
    })?;

    // An existing table always has at least one column, so an empty result means
    // the table does not exist.
    if info.is_empty() {
        return Ok(ShowOutcome::NoSuchTable(show.table.clone()));
    }

    // `PRAGMA table_info` reports the bare type name and drops any declared
    // length (e.g. `varchar(60)` becomes `varchar`). MySQL clients parse that
    // length out of `SHOW COLUMNS` to bound string values — notably `wpdb`,
    // which otherwise reads a length of 0 and truncates every string, aborting
    // `$wpdb->insert()`. Recover the declared sizes from the stored CREATE TABLE
    // text and restore them on the reported types.
    let declared = declared_column_types(conn, &show.table);
    for col in &mut info {
        if let Some(ty) = declared.get(&col.name.to_ascii_lowercase()) {
            col.ty = ty.clone();
        }
    }

    // The `Key` flag (`UNI`/`MUL`) for columns that lead a non-primary index, so
    // `SHOW COLUMNS` matches MySQL (a primary-key column reports `PRI` in
    // `into_row`).
    let lead_keys = lead_column_keys(conn, &show.table);
    for col in &mut info {
        if let Some(k) = lead_keys.get(&col.name.to_ascii_lowercase()) {
            col.key = k;
        }
    }

    let columns: Vec<&'static str> = if show.full {
        COLUMNS_FULL.to_vec()
    } else {
        COLUMNS_BASE.to_vec()
    };

    let mut rows = info
        .into_iter()
        .map(|c| c.into_row(show.full))
        .collect::<Vec<_>>();

    // Apply an optional `LIKE 'pat'` (on `Field`) or `WHERE col {= | LIKE} value`
    // filter to the built rows.
    if let Some(f) = &show.filter {
        let col_idx = columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&f.column))
            .expect("column validated at parse time");
        rows.retain(|row| match row[col_idx].as_deref() {
            // A NULL output column matches neither `=` nor `LIKE`, as in MySQL.
            None => false,
            Some(v) if f.like => like_match(&f.value, v),
            Some(v) => v == f.value,
        });
    }

    Ok(ShowOutcome::Columns(ColumnsResult { columns, rows }))
}

/// Reads the declared column types — including any length/precision such as
/// `varchar(60)` — from the table's stored `CREATE TABLE` text, which the
/// engine preserves even though `PRAGMA table_info` does not. Returns a map from
/// lowercased column name to a size-bearing type string (e.g. `varchar(60)`);
/// columns without a declared size are absent and keep their `PRAGMA` type.
/// Best-effort: any failure to read the schema yields an empty map.
///
/// The column list is scanned directly rather than parsed: the engine renders
/// it with SQLite-isms (e.g. `PRIMARY KEY (ID AUTOINCREMENT)`) that the MySQL
/// front-end parser rejects, and only each column's leading `name type(size)` is
/// needed, so the rest of every definition is ignored.
/// Normalizes a column default for `SHOW COLUMNS`. The engine's
/// `PRAGMA table_info` reports a string default as its SQL literal (`'hi'`,
/// `''`), while MySQL reports the bare value (`hi`, the empty string), so strip
/// the surrounding single quotes and unescape `''` → `'`. Numeric, keyword
/// (`CURRENT_TIMESTAMP`), and `NULL` defaults pass through unchanged.
fn normalize_default(default: Option<String>) -> Option<String> {
    let value = default?;
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        Some(value[1..value.len() - 1].replace("''", "'"))
    } else {
        Some(value)
    }
}

/// Normalizes a column type for `SHOW COLUMNS`, matching MySQL 8.0's display:
/// the type is lowercased, `integer` is rendered as `int`, and the **display
/// width** of an integer type is stripped (`int(11)` → `int`, `bigint(20)
/// unsigned` → `bigint unsigned`) — except `tinyint(1)` (kept, the canonical
/// boolean) and any `zerofill` column (MySQL keeps the width there). Non-integer
/// types (`varchar(60)`, `decimal(10,2)`, …) are returned lowercased, unchanged.
fn normalize_column_type(ty: &str) -> String {
    let lower = ty.trim().to_ascii_lowercase();
    let base_end = lower
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(lower.len());
    let base = if &lower[..base_end] == "integer" {
        "int"
    } else {
        &lower[..base_end]
    };
    if !matches!(base, "int" | "tinyint" | "smallint" | "mediumint" | "bigint") {
        return lower;
    }

    // The remainder after the base is `[(width)] [unsigned] [zerofill]`.
    let rest = &lower[base_end..];
    let (width, suffix) = match (rest.find('('), rest.find(')')) {
        (Some(open), Some(close)) if close > open => {
            (Some(&rest[open + 1..close]), rest[close + 1..].trim())
        }
        _ => (None, rest.trim()),
    };
    let keep_width =
        suffix.contains("zerofill") || (base == "tinyint" && width == Some("1"));

    let mut out = base.to_string();
    if keep_width {
        if let Some(w) = width {
            out.push('(');
            out.push_str(w);
            out.push(')');
        }
    }
    if !suffix.is_empty() {
        out.push(' ');
        out.push_str(suffix);
    }
    out
}

/// For each column that is the leading column of a non-primary index, its MySQL
/// `Key` flag: `UNI` when it leads a unique index, `MUL` when it leads a
/// non-unique one (`UNI` wins if it leads both). Keyed by lowercased column name.
/// The primary-key index (`origin = pk`) is excluded — those columns report
/// `PRI` directly. Only the leading column of each index is recorded, as MySQL
/// flags only that one.
fn lead_column_keys(conn: &Arc<Connection>, table: &str) -> HashMap<String, &'static str> {
    let mut keys: HashMap<String, &'static str> = HashMap::new();
    let escaped = table.replace('\'', "''");

    // `index_list`: seq, name, unique, origin, partial. Skip the PK index.
    let mut indexes: Vec<(String, bool)> = Vec::new();
    let index_list = format!("PRAGMA index_list('{escaped}')");
    if let Ok(Some(mut stmt)) = conn.query(&index_list) {
        let _ = stmt.run_with_row_callback(|row| {
            let name = value_to_string(row.get_value(1)).unwrap_or_default();
            let unique = value_to_string(row.get_value(2)).as_deref() == Some("1");
            let origin = value_to_string(row.get_value(3)).unwrap_or_default();
            if !origin.eq_ignore_ascii_case("pk") {
                indexes.push((name, unique));
            }
            Ok(())
        });
    }

    for (name, unique) in indexes {
        // `index_info`: seqno, cid, name — ordered, so the first row is the lead.
        let idx = name.replace('\'', "''");
        let mut lead: Option<String> = None;
        let index_info = format!("PRAGMA index_info('{idx}')");
        if let Ok(Some(mut stmt)) = conn.query(&index_info) {
            let _ = stmt.run_with_row_callback(|row| {
                if lead.is_none() {
                    lead = value_to_string(row.get_value(2));
                }
                Ok(())
            });
        }
        if let Some(col) = lead {
            let entry = keys.entry(col.to_ascii_lowercase()).or_insert("MUL");
            if unique {
                *entry = "UNI";
            }
        }
    }
    keys
}

fn declared_column_types(conn: &Arc<Connection>, table: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let query = format!(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = '{}'",
        table.replace('\'', "''")
    );
    let Ok(Some(mut stmt)) = conn.query(&query) else {
        return out;
    };
    let mut create_sql = None;
    let _ = stmt.run_with_row_callback(|row| {
        create_sql = value_to_string(row.get_value(0));
        Ok(())
    });
    let Some(create_sql) = create_sql else {
        return out;
    };
    // The column list is the contents of the first parenthesized group.
    let Some(open) = create_sql.find('(') else {
        return out;
    };
    for segment in split_column_list(&create_sql[open + 1..]) {
        if let Some((name, ty)) = parse_column_def(&segment) {
            out.insert(name, ty);
        }
    }
    out
}

/// Splits a `CREATE TABLE` column list into its top-level comma-separated
/// definitions, respecting nested parentheses (type sizes, `PRIMARY KEY (...)`)
/// and single-quoted string literals (default values). Stops at the closing
/// parenthesis that ends the column list.
fn split_column_list(body: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if in_string => {
                // A doubled `''` is an escaped quote, not the end of the string.
                if chars.peek() == Some(&'\'') {
                    current.push(c);
                    current.push(chars.next().unwrap());
                } else {
                    in_string = false;
                    current.push(c);
                }
            }
            '\'' => {
                in_string = true;
                current.push(c);
            }
            _ if in_string => current.push(c),
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' if depth == 0 => break, // closes the column list
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                segments.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        segments.push(current);
    }
    segments
}

/// Extracts `(lowercased column name, size-bearing type)` from one column
/// definition, e.g. `user_login varchar (60) NOT NULL` yields
/// `("user_login", "varchar(60)")`. Returns `None` for a table constraint
/// (`PRIMARY KEY (...)`, `UNIQUE (...)`, ...) or a column with no declared size.
fn parse_column_def(segment: &str) -> Option<(String, String)> {
    let segment = segment.trim();
    let mut rest = segment;

    // Column name: a bare or backtick/quote-delimited identifier.
    let name = if let Some(after) = rest.strip_prefix('`') {
        let end = after.find('`')?;
        rest = &after[end + 1..];
        after[..end].to_string()
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '(')
            .unwrap_or(rest.len());
        let name = rest[..end].to_string();
        rest = &rest[end..];
        name
    };
    let name_lower = name.to_ascii_lowercase();
    if is_table_constraint_keyword(&name_lower) {
        return None;
    }

    // Type name follows the column name.
    let rest = rest.trim_start();
    let type_end = rest
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(rest.len());
    let type_name = &rest[..type_end];
    if type_name.is_empty() {
        return None;
    }

    // Optional `(size)` — possibly separated from the type name by spaces, as
    // the engine renders it (`varchar (60)`).
    let after_type = rest[type_end..].trim_start();
    let size: String = after_type
        .strip_prefix('(')
        .and_then(|s| s.find(')').map(|end| &s[..end]))?
        // Drop all whitespace so a multi-argument size renders like MySQL
        // (`decimal(10,2)`, not the engine's `decimal (10, 2)`).
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if size.is_empty() {
        return None;
    }
    Some((name_lower, format!("{type_name}({size})")))
}

/// Whether `word` (already lowercased) begins a table-level constraint rather
/// than a column definition.
fn is_table_constraint_keyword(word: &str) -> bool {
    matches!(
        word,
        "primary"
            | "unique"
            | "key"
            | "index"
            | "fulltext"
            | "spatial"
            | "foreign"
            | "constraint"
            | "check"
    )
}

/// The parsed form of a `SHOW [FULL] TABLES [LIKE 'pat']` statement.
struct ShowTables {
    full: bool,
    like: Option<String>,
}

/// Selects user base tables from the engine schema for `SHOW TABLES` / `SHOW
/// TABLE STATUS`, excluding both SQLite's internal tables (`sqlite_%`) and
/// turso's internal bookkeeping tables — the `__turso_internal_*` AUTO_INCREMENT
/// sequence and CREATE TYPE tables — which a real MySQL server never exposes.
/// The `__turso_internal_*` underscores are escaped (`ESCAPE '\'`) so `LIKE`
/// treats them literally rather than as single-character wildcards.
const BASE_TABLES_QUERY: &str = "SELECT name FROM sqlite_schema WHERE type = 'table' \
     AND name NOT LIKE 'sqlite_%' \
     AND name NOT LIKE '\\_\\_turso\\_internal\\_%' ESCAPE '\\'";

/// Lists base table names from the schema, optionally filtered by a `LIKE`
/// pattern, as a MySQL `SHOW [FULL] TABLES` result set.
fn build_tables(conn: &Arc<Connection>, show: &ShowTables) -> Result<ColumnsResult, LimboError> {
    let mut query = BASE_TABLES_QUERY.to_string();
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

/// A single-predicate filter on the `SHOW INDEX` output, from a trailing
/// `WHERE col {= | LIKE} value` clause.
struct IndexFilter {
    /// The output column matched against (one of [`INDEX_COLUMNS`]).
    column: String,
    /// `true` for `LIKE` (wildcard match), `false` for `=` (exact match).
    like: bool,
    value: String,
}

/// The parsed form of a `SHOW {INDEX|INDEXES|KEYS} {FROM|IN} tbl` statement.
struct ShowIndex {
    table: String,
    filter: Option<IndexFilter>,
}

/// Strips the surrounding quotes from a string-literal token (`'a_key'` /
/// `"a_key"`), un-doubling any escaped inner quote; returns `None` if the token
/// is not quoted.
fn unquote_token(tok: &str) -> Option<String> {
    tok.strip_prefix('\'')
        .and_then(|p| p.strip_suffix('\''))
        .map(|s| s.replace("''", "'"))
        .or_else(|| {
            tok.strip_prefix('"')
                .and_then(|p| p.strip_suffix('"'))
                .map(|s| s.replace("\"\"", "\""))
        })
}

/// Parses `SHOW {INDEX|INDEXES|KEYS} {FROM|IN} tbl [{FROM|IN} db]`, with an
/// optional trailing `WHERE col {= | LIKE} value` filter (MySQL filters the
/// output rows; WordPress's `dbDelta()` issues `... WHERE Key_name='a_key'`).
/// Only a single `=`/`LIKE` predicate over one known output column is
/// recognized; a more complex `WHERE` (or an unknown column) returns `None`, so
/// it falls through to the parser as unsupported rather than silently matching.
fn parse_show_index(sql: &str) -> Option<ShowIndex> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let toks = tokenize(trimmed);
    let mut k = 0;
    let kw = |t: &str, kw: &str| t.eq_ignore_ascii_case(kw);

    if !toks.get(k).is_some_and(|t| kw(t, "SHOW")) {
        return None;
    }
    k += 1;
    if !toks
        .get(k)
        .is_some_and(|t| kw(t, "INDEX") || kw(t, "INDEXES") || kw(t, "KEYS"))
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
    // Optional `WHERE col {= | LIKE} value` filter over a single output column.
    let filter = if toks.get(k).is_some_and(|t| kw(t, "WHERE")) {
        k += 1;
        let column = toks.get(k)?.clone();
        // Only a known output column is accepted; anything else falls through.
        if !INDEX_COLUMNS.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
            return None;
        }
        k += 1;
        let like = match toks.get(k)?.as_str() {
            "=" => false,
            t if kw(t, "LIKE") => true,
            _ => return None,
        };
        k += 1;
        let raw = toks.get(k)?;
        // A quoted value is unquoted; a bare token (e.g. `Non_unique = 0`) is
        // taken verbatim, matching the string form the row holds.
        let value = unquote_token(raw).unwrap_or_else(|| raw.clone());
        k += 1;
        Some(IndexFilter {
            column,
            like,
            value,
        })
    } else {
        None
    };

    // Any further tokens (a compound `WHERE ... AND ...`, etc.) are unsupported.
    if k != toks.len() {
        return None;
    }
    Some(ShowIndex { table, filter })
}

/// The 15 columns of MySQL 8's `SHOW INDEX` result set, in order.
const INDEX_COLUMNS: [&str; 15] = [
    "Table",
    "Non_unique",
    "Key_name",
    "Seq_in_index",
    "Column_name",
    "Collation",
    "Cardinality",
    "Sub_part",
    "Packed",
    "Null",
    "Index_type",
    "Comment",
    "Index_comment",
    "Visible",
    "Expression",
];

/// Builds one `SHOW INDEX` row for the `seq`-th (1-based) column of an index.
fn index_row(
    table: &str,
    non_unique: &str,
    key_name: &str,
    seq: usize,
    column: &str,
    null: &str,
) -> Vec<Option<String>> {
    vec![
        Some(table.to_string()),
        Some(non_unique.to_string()),
        Some(key_name.to_string()),
        Some(seq.to_string()),
        Some(column.to_string()),
        Some("A".to_string()), // Collation (ascending)
        None,                  // Cardinality (the engine has no index statistics)
        None,                  // Sub_part (no prefix length tracked)
        None,                  // Packed
        Some(null.to_string()),
        Some("BTREE".to_string()), // Index_type
        Some(String::new()),       // Comment
        Some(String::new()),       // Index_comment
        Some("YES".to_string()),   // Visible
        None,                      // Expression
    ]
}

/// Reads the table's indexes via `PRAGMA index_list` / `index_info` and reshapes
/// them into the MySQL `SHOW INDEX` result set. The engine keeps no index
/// statistics, so `Cardinality` is reported as NULL (likewise `Sub_part`,
/// `Packed`, `Expression`); `Collation` is `A`, `Index_type` is `BTREE`, and
/// `Visible` is `YES`. A primary key is reported under MySQL's `PRIMARY` name,
/// including the rowid-alias integer primary key the engine keeps out of
/// `index_list`. `dbDelta()` reads this output to learn which indexes exist.
fn build_index(conn: &Arc<Connection>, show: &ShowIndex) -> Result<ShowOutcome, LimboError> {
    let tbl = show.table.replace('\'', "''");

    // `table_info` detects a missing table and gives nullability and the PK columns.
    let table_info = format!("PRAGMA table_info('{tbl}')");
    let Some(mut stmt) = conn.query(&table_info)? else {
        return Ok(ShowOutcome::NoSuchTable(show.table.clone()));
    };
    let mut nullable: HashMap<String, bool> = HashMap::new();
    let mut pk_cols: Vec<(i64, String)> = Vec::new();
    let mut any = false;
    stmt.run_with_row_callback(|row| {
        any = true;
        let name = value_to_string(row.get_value(1)).unwrap_or_default();
        let notnull = value_to_string(row.get_value(3)).as_deref() != Some("0");
        let pk = value_to_string(row.get_value(5))
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        if pk > 0 {
            pk_cols.push((pk, name.clone()));
        }
        nullable.insert(name, !notnull);
        Ok(())
    })?;
    if !any {
        return Ok(ShowOutcome::NoSuchTable(show.table.clone()));
    }
    pk_cols.sort_by_key(|(ord, _)| *ord);

    let null_flag = |col: &str| {
        if *nullable.get(col).unwrap_or(&true) {
            "YES"
        } else {
            ""
        }
    };

    // `index_list`: seq, name, unique, origin, partial.
    let mut idxs: Vec<(String, bool, String)> = Vec::new();
    let index_list = format!("PRAGMA index_list('{tbl}')");
    if let Some(mut stmt) = conn.query(&index_list)? {
        stmt.run_with_row_callback(|row| {
            let name = value_to_string(row.get_value(1)).unwrap_or_default();
            let unique = value_to_string(row.get_value(2)).as_deref() == Some("1");
            let origin = value_to_string(row.get_value(3)).unwrap_or_default();
            idxs.push((name, unique, origin));
            Ok(())
        })?;
    }

    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut have_pk_index = false;
    for (name, unique, origin) in &idxs {
        let is_pk = origin.eq_ignore_ascii_case("pk");
        let key_name = if is_pk {
            have_pk_index = true;
            "PRIMARY".to_string()
        } else {
            name.clone()
        };
        let non_unique = if *unique { "0" } else { "1" };
        // `index_info`: seqno, cid, name (the indexed column).
        let mut cols: Vec<String> = Vec::new();
        let idx = name.replace('\'', "''");
        let index_info = format!("PRAGMA index_info('{idx}')");
        if let Some(mut stmt) = conn.query(&index_info)? {
            stmt.run_with_row_callback(|row| {
                cols.push(value_to_string(row.get_value(2)).unwrap_or_default());
                Ok(())
            })?;
        }
        for (i, col) in cols.iter().enumerate() {
            // A primary key's columns are never nullable in MySQL.
            let null = if is_pk { "" } else { null_flag(col) };
            rows.push(index_row(
                &show.table,
                non_unique,
                &key_name,
                i + 1,
                col,
                null,
            ));
        }
    }

    // A single-column INTEGER PRIMARY KEY is a rowid alias and never appears in
    // `index_list`, so synthesize its `PRIMARY` rows from `table_info`.
    if !have_pk_index {
        for (i, (_, col)) in pk_cols.iter().enumerate() {
            rows.push(index_row(&show.table, "0", "PRIMARY", i + 1, col, ""));
        }
    }

    // Apply an optional `WHERE col {= | LIKE} value` filter to the built rows.
    if let Some(f) = &show.filter {
        let col_idx = INDEX_COLUMNS
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&f.column))
            .expect("column validated at parse time");
        rows.retain(|row| match row[col_idx].as_deref() {
            // A NULL output column matches neither `=` nor `LIKE`, as in MySQL.
            None => false,
            Some(v) if f.like => like_match(&f.value, v),
            Some(v) => v == f.value,
        });
    }

    Ok(ShowOutcome::Columns(ColumnsResult {
        columns: INDEX_COLUMNS.to_vec(),
        rows,
    }))
}

/// One column as read from `PRAGMA table_info`.
struct ColumnInfo {
    name: String,
    ty: String,
    notnull: bool,
    default: Option<String>,
    pk: bool,
    /// The MySQL `Key` flag for a non-primary-key column: `UNI` when the column
    /// leads a unique index, `MUL` when it leads a non-unique one, else empty.
    /// A primary-key column reports `PRI` regardless (see [`Self::into_row`]).
    key: &'static str,
}

impl ColumnInfo {
    /// Reshapes the column into a MySQL `SHOW [FULL] COLUMNS` row.
    fn into_row(self, full: bool) -> Vec<Option<String>> {
        // A primary-key column is always NOT NULL in MySQL, even when the engine
        // does not flag it (an `INTEGER PRIMARY KEY` rowid alias reports
        // `notnull = 0` in `PRAGMA table_info`).
        let null = if self.notnull || self.pk { "NO" } else { "YES" };
        // A primary-key column is `PRI`; otherwise `UNI`/`MUL` from leading an
        // index, or empty.
        let key = if self.pk { "PRI" } else { self.key };
        let collation = if is_text_type(&self.ty) {
            Some(COLLATION.to_string())
        } else {
            None
        };
        // MySQL lowercases the type and strips the display width of integer
        // types (`int(11)` → `int`), which WordPress's dbDelta compares.
        let ty = Some(normalize_column_type(&self.ty));
        // MySQL reports a string default as its bare value (`hi`), where the
        // engine stores the SQL literal (`'hi'`); strip the quotes.
        let default = normalize_default(self.default);
        let field = Some(self.name);
        if full {
            vec![
                field,
                ty,
                collation,
                Some(null.to_string()),
                Some(key.to_string()),
                default,
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
                default,
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

/// Quotes an identifier for engine SQL with double quotes, doubling any embedded
/// quote (`a"b` → `"a""b"`).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Makes the engine accept an `ALTER TABLE <table> DROP COLUMN <column>` even when
/// an index covers the column, matching MySQL.
///
/// MySQL drops a column that participates in an index by adjusting the indexes: a
/// single-column index on the column is dropped outright, and a multi-column
/// index survives over its remaining columns. The engine instead refuses to drop
/// a column referenced by any index ("cannot drop column ...: it is referenced in
/// the index ..."), so this runs first and, for every index covering `column`,
/// drops it and — when other columns remain — recreates it without `column`. The
/// primary-key index is left untouched (dropping a primary-key column is a
/// distinct, restricted operation, and a rowid primary key has no droppable
/// index).
///
/// Best effort: any introspection or DDL step that fails is ignored, so the
/// caller's `DROP COLUMN` still runs and surfaces the real error (e.g. an unknown
/// table or a column that does not exist). The index rewrites are not atomic with
/// the column drop — like the front-end's other `ALTER TABLE` emulations.
pub(crate) fn adjust_indexes_for_dropped_column(
    conn: &Arc<Connection>,
    table: &str,
    column: &str,
) {
    // `index_list`: seq, name, unique, origin, partial.
    let mut indexes: Vec<(String, bool, String)> = Vec::new();
    let list_sql = format!("PRAGMA index_list('{}')", table.replace('\'', "''"));
    if let Ok(Some(mut stmt)) = conn.query(&list_sql) {
        let _ = stmt.run_with_row_callback(|row| {
            let name = value_to_string(row.get_value(1)).unwrap_or_default();
            let unique = value_to_string(row.get_value(2)).as_deref() == Some("1");
            let origin = value_to_string(row.get_value(3)).unwrap_or_default();
            indexes.push((name, unique, origin));
            Ok(())
        });
    }

    for (name, unique, origin) in &indexes {
        // The primary-key index is not adjusted here.
        if origin.eq_ignore_ascii_case("pk") {
            continue;
        }
        // `index_info`: seqno, cid, column.
        let mut cols: Vec<String> = Vec::new();
        let info_sql = format!("PRAGMA index_info('{}')", name.replace('\'', "''"));
        if let Ok(Some(mut stmt)) = conn.query(&info_sql) {
            let _ = stmt.run_with_row_callback(|row| {
                cols.push(value_to_string(row.get_value(2)).unwrap_or_default());
                Ok(())
            });
        }
        if !cols.iter().any(|c| c.eq_ignore_ascii_case(column)) {
            continue;
        }

        let _ = conn.execute(format!("DROP INDEX {}", quote_ident(name)));
        let remaining: Vec<&String> = cols
            .iter()
            .filter(|c| !c.eq_ignore_ascii_case(column))
            .collect();
        if !remaining.is_empty() {
            let cols_sql = remaining
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            let unique_kw = if *unique { "UNIQUE " } else { "" };
            let _ = conn.execute(format!(
                "CREATE {unique_kw}INDEX {} ON {} ({cols_sql})",
                quote_ident(name),
                quote_ident(table),
            ));
        }
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

    // Optional `LIKE 'pat'` (matched against `Field`, the column name — the form
    // WordPress uses to check whether a column exists) or `WHERE col {= | LIKE}
    // value` over a single known output column.
    let filter = if toks.get(k).is_some_and(|t| kw(t, "LIKE")) {
        k += 1;
        let pat = unquote_token(toks.get(k)?)?;
        k += 1;
        Some(ColumnFilter {
            column: "Field".to_string(),
            like: true,
            value: pat,
        })
    } else if toks.get(k).is_some_and(|t| kw(t, "WHERE")) {
        k += 1;
        let column = toks.get(k)?.clone();
        let known = if full { &COLUMNS_FULL[..] } else { &COLUMNS_BASE[..] };
        if !known.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
            return None;
        }
        k += 1;
        let like = match toks.get(k)?.as_str() {
            "=" => false,
            t if kw(t, "LIKE") => true,
            _ => return None,
        };
        k += 1;
        let raw = toks.get(k)?;
        let value = unquote_token(raw).unwrap_or_else(|| raw.clone());
        k += 1;
        Some(ColumnFilter {
            column,
            like,
            value,
        })
    } else {
        None
    };

    // Any further tokens (a compound `WHERE ... AND ...`, etc.) are unsupported.
    if k != toks.len() {
        return None;
    }
    Some(ShowColumns {
        full,
        table,
        filter,
    })
}

/// Parses `{DESCRIBE | DESC} tbl` (optionally `db.tbl`), MySQL's synonym for
/// `SHOW COLUMNS FROM tbl`. DESCRIBE always yields the non-FULL six-column
/// shape. The `DESCRIBE tbl col_name` / `DESCRIBE tbl 'wild'` column-filter
/// forms are not handled here, so they fall through (and are rejected as
/// unsupported); WordPress only issues the bare `DESCRIBE tbl` form.
fn parse_describe(sql: &str) -> Option<ShowColumns> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let toks = tokenize(trimmed);
    let mut k = 0;

    let kw = |t: &str, kw: &str| t.eq_ignore_ascii_case(kw);

    if !toks
        .get(k)
        .is_some_and(|t| kw(t, "DESCRIBE") || kw(t, "DESC"))
    {
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

    // A trailing column name or wildcard (DESCRIBE tbl col) is not handled here.
    if k != toks.len() {
        return None;
    }
    Some(ShowColumns {
        full: false,
        table,
        filter: None,
    })
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

/// The 18 columns of MySQL 8's `SHOW TABLE STATUS` result set, in order.
const TABLE_STATUS_COLUMNS: &[&str] = &[
    "Name",
    "Engine",
    "Version",
    "Row_format",
    "Rows",
    "Avg_row_length",
    "Data_length",
    "Max_data_length",
    "Index_length",
    "Data_free",
    "Auto_increment",
    "Create_time",
    "Update_time",
    "Check_time",
    "Collation",
    "Checksum",
    "Create_options",
    "Comment",
];

/// The parsed form of a `SHOW TABLE STATUS [{FROM|IN} db] [LIKE 'pattern']`
/// statement: the optional `LIKE` pattern (`None` lists every table).
struct ShowTableStatus {
    /// A filter on the table name: `(is_like, value)`. From `LIKE 'pat'`
    /// (is_like) or `WHERE Name {= | LIKE} value`.
    name_filter: Option<(bool, String)>,
}

/// Parses `SHOW TABLE STATUS [{FROM|IN} db] [LIKE 'pattern']`, returning `None`
/// for any other statement (including the `WHERE` form, which falls through).
fn parse_show_table_status(sql: &str) -> Option<ShowTableStatus> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let toks = tokenize(trimmed);
    let mut k = 0;
    let kw = |t: &str, kw: &str| t.eq_ignore_ascii_case(kw);

    if !toks.get(k).is_some_and(|t| kw(t, "SHOW")) {
        return None;
    }
    k += 1;
    if !toks.get(k).is_some_and(|t| kw(t, "TABLE")) {
        return None;
    }
    k += 1;
    if !toks.get(k).is_some_and(|t| kw(t, "STATUS")) {
        return None;
    }
    k += 1;

    // Optional `{FROM|IN} db` qualifier; consumed and ignored.
    if toks.get(k).is_some_and(|t| kw(t, "FROM") || kw(t, "IN")) {
        k += 1;
        toks.get(k)?;
        k += 1;
    }

    // An optional table-name filter: `LIKE 'pattern'`, or `WHERE Name {= | LIKE}
    // value` (WordPress's `wpdb` issues `SHOW TABLE STATUS WHERE Name = 'tbl'`).
    let name_filter = if toks.get(k).is_some_and(|t| kw(t, "LIKE")) {
        k += 1;
        let value = unquote_token(toks.get(k)?)?;
        k += 1;
        Some((true, value))
    } else if toks.get(k).is_some_and(|t| kw(t, "WHERE")) {
        k += 1;
        // Only a single `Name {= | LIKE} value` predicate is recognized.
        if !toks.get(k).is_some_and(|t| kw(t, "Name")) {
            return None;
        }
        k += 1;
        let like = match toks.get(k)?.as_str() {
            "=" => false,
            t if kw(t, "LIKE") => true,
            _ => return None,
        };
        k += 1;
        let value = unquote_token(toks.get(k)?)?;
        k += 1;
        Some((like, value))
    } else {
        None
    };

    if k != toks.len() {
        return None;
    }
    Some(ShowTableStatus { name_filter })
}

/// Builds the `SHOW TABLE STATUS` result set from the schema. Most columns the
/// engine does not track (sizes, timestamps, auto-increment) are reported as `0`
/// or NULL; `Engine`/`Row_format`/`Collation` are the front-end's fixed values,
/// and `Rows` is the table's real `COUNT(*)`. WordPress reads `Collation` (to
/// derive a table's charset) and sums `Data_length`/`Index_length`.
fn build_table_status(
    conn: &Arc<Connection>,
    show: &ShowTableStatus,
) -> Result<ColumnsResult, LimboError> {
    let mut query = BASE_TABLES_QUERY.to_string();
    if let Some((is_like, value)) = &show.name_filter {
        let escaped = value.replace('\'', "''");
        if *is_like {
            // MySQL's `LIKE` uses backslash as the default escape character (so a
            // WordPress pattern like `wp\_%` matches a literal underscore); the
            // engine's `LIKE` has none, so supply `ESCAPE '\'`.
            query.push_str(&format!(" AND name LIKE '{escaped}' ESCAPE '\\'"));
        } else {
            query.push_str(&format!(" AND name = '{escaped}'"));
        }
    }
    query.push_str(" ORDER BY name");

    let mut names: Vec<String> = Vec::new();
    if let Some(mut stmt) = conn.query(&query)? {
        stmt.run_with_row_callback(|row| {
            names.push(value_to_string(row.get_value(0)).unwrap_or_default());
            Ok(())
        })?;
    }

    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    for name in names {
        let count = table_row_count(conn, &name)?;
        rows.push(vec![
            Some(name),
            Some("InnoDB".to_string()),
            Some("10".to_string()),
            Some("Dynamic".to_string()),
            Some(count.to_string()),
            Some("0".to_string()), // Avg_row_length
            Some("0".to_string()), // Data_length
            Some("0".to_string()), // Max_data_length
            Some("0".to_string()), // Index_length
            Some("0".to_string()), // Data_free
            None,                  // Auto_increment
            None,                  // Create_time
            None,                  // Update_time
            None,                  // Check_time
            Some(COLLATION.to_string()),
            None,                // Checksum
            Some(String::new()), // Create_options
            Some(String::new()), // Comment
        ]);
    }

    Ok(ColumnsResult {
        columns: TABLE_STATUS_COLUMNS.to_vec(),
        rows,
    })
}

/// Counts the rows of `table` (for the `Rows` column of `SHOW TABLE STATUS`).
fn table_row_count(conn: &Arc<Connection>, table: &str) -> Result<i64, LimboError> {
    let query = format!("SELECT COUNT(*) FROM \"{}\"", table.replace('"', "\"\""));
    let mut count = 0i64;
    if let Some(mut stmt) = conn.query(&query)? {
        stmt.run_with_row_callback(|row| {
            count = value_to_string(row.get_value(0))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            Ok(())
        })?;
    }
    Ok(count)
}

/// The parsed form of a `SHOW [GLOBAL|SESSION] VARIABLES [LIKE 'pattern']`
/// statement: the optional `LIKE` pattern (`None` for the bare form).
struct ShowVariables {
    like: Option<String>,
}

/// Parses `SHOW [GLOBAL|SESSION] VARIABLES [LIKE 'pattern']`, returning `None`
/// for any other statement — including the `SHOW VARIABLES WHERE ...` form,
/// which carries an arbitrary predicate and is left to fall through.
fn parse_show_variables(sql: &str) -> Option<ShowVariables> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let toks = tokenize(trimmed);
    let mut k = 0;
    let kw = |t: &str, kw: &str| t.eq_ignore_ascii_case(kw);

    if !toks.get(k).is_some_and(|t| kw(t, "SHOW")) {
        return None;
    }
    k += 1;
    // An optional `GLOBAL`/`SESSION` scope; the front-end keeps a single set of
    // values, so the scope is consumed and ignored.
    if toks.get(k).is_some_and(|t| kw(t, "GLOBAL") || kw(t, "SESSION")) {
        k += 1;
    }
    if !toks.get(k).is_some_and(|t| kw(t, "VARIABLES")) {
        return None;
    }
    k += 1;

    // Optional `LIKE 'pattern'`.
    let like = if toks.get(k).is_some_and(|t| kw(t, "LIKE")) {
        k += 1;
        let pat = toks.get(k)?;
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
    Some(ShowVariables { like })
}

/// Builds the `SHOW VARIABLES` result set (`Variable_name`, `Value`) from the
/// shared system-variable table, filtered by the optional `LIKE` pattern and
/// ordered by name as MySQL reports it.
fn build_variables(show: &ShowVariables) -> ColumnsResult {
    let mut rows: Vec<Vec<Option<String>>> = crate::session::SYSTEM_VARIABLES
        .iter()
        .filter(|(name, _)| match &show.like {
            Some(pattern) => like_match(pattern, name),
            None => true,
        })
        .map(|(name, value)| vec![Some((*name).to_string()), Some((*value).to_string())])
        .collect();
    rows.sort_by(|a, b| a[0].cmp(&b[0]));
    ColumnsResult {
        columns: vec!["Variable_name", "Value"],
        rows,
    }
}

/// Case-insensitive SQL `LIKE` match for `SHOW ... LIKE` patterns: `%` matches
/// any sequence (including empty) and `_` matches any single character. There is
/// no escape handling — variable-name patterns never need it.
fn like_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    // Classic two-pointer wildcard match with backtracking on `%`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut star_ti): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
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
        } else if matches!(c, '.' | ',' | ';' | '(' | ')' | '=') {
            chars.next();
            toks.push(c.to_string());
        } else {
            let mut w = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace()
                    || matches!(ch, '`' | '\'' | '"' | '.' | ',' | ';' | '(' | ')' | '=')
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
    fn normalizes_string_default() {
        // String defaults lose their surrounding quotes; `''` → empty.
        assert_eq!(normalize_default(Some("'hi'".to_string())), Some("hi".to_string()));
        assert_eq!(normalize_default(Some("''".to_string())), Some(String::new()));
        assert_eq!(
            normalize_default(Some("'a''b'".to_string())),
            Some("a'b".to_string())
        );
        // Numeric, keyword, and NULL defaults are unchanged.
        assert_eq!(normalize_default(Some("0".to_string())), Some("0".to_string()));
        assert_eq!(normalize_default(Some("5".to_string())), Some("5".to_string()));
        assert_eq!(
            normalize_default(Some("CURRENT_TIMESTAMP".to_string())),
            Some("CURRENT_TIMESTAMP".to_string())
        );
        assert_eq!(normalize_default(None), None);
    }

    #[test]
    fn normalizes_integer_display_width() {
        // Integer display widths are stripped, like MySQL 8.0.
        assert_eq!(normalize_column_type("INT(11)"), "int");
        assert_eq!(normalize_column_type("int(11) unsigned"), "int unsigned");
        assert_eq!(normalize_column_type("BIGINT(20) UNSIGNED"), "bigint unsigned");
        assert_eq!(normalize_column_type("smallint(6)"), "smallint");
        assert_eq!(normalize_column_type("mediumint(9)"), "mediumint");
        assert_eq!(normalize_column_type("integer"), "int");
        assert_eq!(normalize_column_type("int"), "int");
        assert_eq!(normalize_column_type("bigint unsigned"), "bigint unsigned");

        // `tinyint(1)` (boolean) and zerofill columns keep the width.
        assert_eq!(normalize_column_type("tinyint(1)"), "tinyint(1)");
        assert_eq!(normalize_column_type("tinyint(4)"), "tinyint");
        assert_eq!(
            normalize_column_type("int(10) unsigned zerofill"),
            "int(10) unsigned zerofill"
        );

        // Non-integer types keep their size, lowercased.
        assert_eq!(normalize_column_type("VARCHAR(60)"), "varchar(60)");
        assert_eq!(normalize_column_type("Decimal(10,2)"), "decimal(10,2)");
        assert_eq!(normalize_column_type("LONGTEXT"), "longtext");
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
    fn base_tables_query_excludes_internal_tables() {
        // The shared schema query both SHOW TABLES and SHOW TABLE STATUS use must
        // filter out SQLite's `sqlite_%` tables and turso's `__turso_internal_*`
        // bookkeeping tables (with the underscores escaped for `LIKE`).
        assert!(BASE_TABLES_QUERY.contains("name NOT LIKE 'sqlite_%'"));
        assert!(BASE_TABLES_QUERY.contains(r"name NOT LIKE '\_\_turso\_internal\_%' ESCAPE '\'"));
    }

    #[test]
    fn parses_show_warnings_and_errors() {
        for sql in [
            "SHOW WARNINGS",
            "SHOW ERRORS",
            "SHOW WARNINGS LIMIT 5",
            "SHOW WARNINGS LIMIT 1, 5",
            "show warnings;",
        ] {
            let result = parse_show_warnings(sql).unwrap_or_else(|| panic!("`{sql}` should parse"));
            assert_eq!(result.columns, vec!["Level", "Code", "Message"]);
            assert!(result.rows.is_empty(), "`{sql}` should have no rows");
        }
        // The `COUNT(*)` form and unrelated SHOWs fall through.
        assert!(parse_show_warnings("SHOW COUNT(*) WARNINGS").is_none());
        assert!(parse_show_warnings("SHOW TABLES").is_none());
    }

    #[test]
    fn parses_show_empty_enumeration() {
        // Each enumerates objects this engine never has, so it is always empty.
        for (sql, first_col, ncols) in [
            ("SHOW TRIGGERS", "Trigger", 11),
            ("SHOW EVENTS", "Db", 15),
            ("SHOW PROCEDURE STATUS", "Db", 12),
            ("SHOW FUNCTION STATUS", "Db", 12),
            // A trailing filter is accepted and irrelevant on an empty set.
            ("SHOW TRIGGERS FROM conf", "Trigger", 11),
            ("SHOW PROCEDURE STATUS LIKE 'wp_%'", "Db", 12),
            ("show function status where db = 'conf';", "Db", 12),
        ] {
            let result =
                parse_show_empty_enumeration(sql).unwrap_or_else(|| panic!("`{sql}` should parse"));
            assert_eq!(result.columns.len(), ncols, "column count for `{sql}`");
            assert_eq!(result.columns[0], first_col);
            assert!(result.rows.is_empty(), "`{sql}` should have no rows");
        }
        // Plain `SHOW STATUS` (runtime counters) and unrelated SHOWs fall through.
        assert!(parse_show_empty_enumeration("SHOW STATUS").is_none());
        assert!(parse_show_empty_enumeration("SHOW STATUS LIKE 'Uptime'").is_none());
        assert!(parse_show_empty_enumeration("SHOW TABLES").is_none());
    }

    #[test]
    fn parses_show_table_status() {
        assert!(parse_show_table_status("SHOW TABLE STATUS")
            .unwrap()
            .name_filter
            .is_none());
        // `LIKE 'pat'` is a like-filter on the name.
        assert_eq!(
            parse_show_table_status("SHOW TABLE STATUS LIKE 'wp_posts'")
                .unwrap()
                .name_filter,
            Some((true, "wp_posts".to_string()))
        );
        // `WHERE Name = 'tbl'` is an exact filter; `WHERE Name LIKE 'pat'` a
        // like-filter (WordPress's `wpdb` issues the `=` form).
        assert_eq!(
            parse_show_table_status("SHOW TABLE STATUS WHERE Name = 'wp_posts'")
                .unwrap()
                .name_filter,
            Some((false, "wp_posts".to_string()))
        );
        assert_eq!(
            parse_show_table_status("SHOW TABLE STATUS WHERE Name LIKE 'wp\\_%'")
                .unwrap()
                .name_filter,
            Some((true, "wp\\_%".to_string()))
        );
        // The `{FROM|IN} db` qualifier is accepted and ignored.
        assert!(parse_show_table_status("SHOW TABLE STATUS FROM mydb LIKE 't'").is_some());
        // Unrelated statements and a non-`Name` WHERE column fall through.
        assert!(parse_show_table_status("SHOW TABLES").is_none());
        assert!(parse_show_table_status("SHOW TABLE STATUS WHERE Engine = 'InnoDB'").is_none());
        // The 18-column MySQL shape is reported in order.
        assert_eq!(TABLE_STATUS_COLUMNS.len(), 18);
        assert_eq!(TABLE_STATUS_COLUMNS[0], "Name");
        assert_eq!(TABLE_STATUS_COLUMNS[14], "Collation");
        assert_eq!(TABLE_STATUS_COLUMNS[17], "Comment");
    }

    #[test]
    fn parses_show_variables() {
        assert_eq!(parse_show_variables("SHOW VARIABLES").unwrap().like, None);
        assert_eq!(
            parse_show_variables("SHOW VARIABLES LIKE 'max_allowed_packet'")
                .unwrap()
                .like
                .as_deref(),
            Some("max_allowed_packet")
        );
        // The GLOBAL / SESSION scope is accepted.
        assert!(parse_show_variables("SHOW GLOBAL VARIABLES LIKE 'autocommit'").is_some());
        assert!(parse_show_variables("SHOW SESSION VARIABLES").is_some());
        // Unrelated statements and the WHERE form fall through.
        assert!(parse_show_variables("SHOW TABLES").is_none());
        assert!(parse_show_variables("SHOW VARIABLES WHERE Variable_name = 'x'").is_none());
    }

    #[test]
    fn build_variables_filters_and_orders() {
        // An exact name yields one row.
        let one = build_variables(&ShowVariables {
            like: Some("autocommit".to_string()),
        });
        assert_eq!(one.rows.len(), 1);
        assert_eq!(one.rows[0][0].as_deref(), Some("autocommit"));
        assert_eq!(one.rows[0][1].as_deref(), Some("1"));

        // A `%` wildcard matches several, ordered by name; `_` matches one char;
        // matching is case-insensitive.
        let many = build_variables(&ShowVariables {
            like: Some("CHARACTER_SET_C%".to_string()),
        });
        let names: Vec<_> = many.rows.iter().map(|r| r[0].clone().unwrap()).collect();
        assert_eq!(names, ["character_set_client", "character_set_connection"]);

        // An unknown variable yields no rows.
        let none = build_variables(&ShowVariables {
            like: Some("no_such_xyzzy".to_string()),
        });
        assert!(none.rows.is_empty());
    }

    #[test]
    fn like_match_semantics() {
        assert!(like_match("autocommit", "autocommit"));
        assert!(like_match("AUTO%", "autocommit"));
        assert!(like_match("autocommi_", "autocommit"));
        assert!(like_match("%commit", "autocommit"));
        assert!(like_match("%", "anything"));
        assert!(!like_match("autocommi_", "autocommit_extra"));
        assert!(!like_match("xyz", "autocommit"));
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
        // A compound WHERE or an unknown column still falls through to the parser.
        assert!(parse_show_columns("SHOW COLUMNS FROM t WHERE Bogus = 'x'").is_none());
        assert!(
            parse_show_columns("SHOW COLUMNS FROM t WHERE Field = 'a' AND Null = 'NO'").is_none()
        );
    }

    #[test]
    fn parses_show_columns_with_filter() {
        // `LIKE 'pat'` filters on the Field (column name).
        let p = parse_show_columns("SHOW COLUMNS FROM t LIKE 'a%'").unwrap();
        let f = p.filter.expect("a filter");
        assert_eq!(f.column, "Field");
        assert!(f.like);
        assert_eq!(f.value, "a%");

        // `WHERE col = 'value'` filters on a named output column.
        let f = parse_show_columns("SHOW COLUMNS FROM t WHERE Field='id'")
            .unwrap()
            .filter
            .expect("a filter");
        assert_eq!(f.column, "Field");
        assert!(!f.like);
        assert_eq!(f.value, "id");

        // `Collation`/`Privileges`/`Comment` are only valid columns in the FULL
        // form, so a WHERE on them parses only there.
        assert!(parse_show_columns("SHOW FULL COLUMNS FROM t WHERE Collation='x'")
            .unwrap()
            .filter
            .is_some());
        assert!(parse_show_columns("SHOW COLUMNS FROM t WHERE Collation='x'").is_none());

        // The bare form has no filter.
        assert!(parse_show_columns("SHOW COLUMNS FROM t")
            .unwrap()
            .filter
            .is_none());
    }

    #[test]
    fn text_types_carry_a_collation() {
        assert!(is_text_type("VARCHAR"));
        assert!(is_text_type("text"));
        assert!(is_text_type("LONGTEXT"));
        assert!(!is_text_type("INT"));
        assert!(!is_text_type("BIGINT"));
    }

    #[test]
    fn parses_maintenance_statements() {
        // Each op yields a `status`/`OK` row per table with the op name.
        let r = parse_maintenance("ANALYZE TABLE t").unwrap();
        assert_eq!(r.columns, vec!["Table", "Op", "Msg_type", "Msg_text"]);
        assert_eq!(
            r.rows,
            vec![vec![
                Some("t".to_string()),
                Some("analyze".to_string()),
                Some("status".to_string()),
                Some("OK".to_string()),
            ]]
        );

        // The op name is taken from the keyword.
        for (sql, op) in [
            ("CHECK TABLE t", "check"),
            ("OPTIMIZE TABLE t", "optimize"),
            ("REPAIR TABLE t", "repair"),
        ] {
            assert_eq!(parse_maintenance(sql).unwrap().rows[0][1], Some(op.to_string()));
        }

        // A comma-separated list gives one row per table; the `LOCAL` modifier
        // and trailing options are ignored; a `db.tbl` name uses the table part.
        assert_eq!(parse_maintenance("ANALYZE TABLE a, b").unwrap().rows.len(), 2);
        assert_eq!(parse_maintenance("OPTIMIZE LOCAL TABLE t").unwrap().rows.len(), 1);
        assert_eq!(parse_maintenance("CHECK TABLE t QUICK").unwrap().rows.len(), 1);
        assert_eq!(
            parse_maintenance("ANALYZE TABLE d.t").unwrap().rows[0][0],
            Some("t".to_string())
        );

        // Unrelated statements fall through.
        assert!(parse_maintenance("SELECT 1").is_none());
        assert!(parse_maintenance("ANALYZE t").is_none()); // missing TABLE
    }

    #[test]
    fn parses_show_index_with_where_filter() {
        // Plain SHOW INDEX has no filter.
        let p = parse_show_index("SHOW INDEX FROM t").unwrap();
        assert_eq!(p.table, "t");
        assert!(p.filter.is_none());

        // `WHERE Key_name='a_key'` (no spaces, as WordPress emits) parses into an
        // exact-match filter with the value unquoted.
        let p = parse_show_index("SHOW INDEX FROM t WHERE Key_name='a_key'").unwrap();
        let f = p.filter.expect("a filter");
        assert_eq!(f.column, "Key_name");
        assert!(!f.like);
        assert_eq!(f.value, "a_key");

        // `LIKE` parses into a wildcard filter; the column match is
        // case-insensitive.
        let p = parse_show_index("SHOW INDEXES FROM t WHERE key_name LIKE 'a%'").unwrap();
        let f = p.filter.expect("a filter");
        assert!(f.like);
        assert_eq!(f.value, "a%");

        // A bare (unquoted) value is taken verbatim.
        let f = parse_show_index("SHOW INDEX FROM t WHERE Non_unique = 0")
            .unwrap()
            .filter
            .expect("a filter");
        assert_eq!(f.value, "0");

        // An unknown column or a compound predicate falls through (None) so the
        // statement is reported unsupported rather than silently mis-filtered.
        assert!(parse_show_index("SHOW INDEX FROM t WHERE Bogus = 'x'").is_none());
        assert!(parse_show_index("SHOW INDEX FROM t WHERE Key_name = 'a' AND Seq_in_index = 1").is_none());
    }
}

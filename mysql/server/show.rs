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

/// The parsed form of a `SHOW {INDEX|INDEXES|KEYS} {FROM|IN} tbl` statement.
struct ShowIndex {
    table: String,
}

/// Parses `SHOW {INDEX|INDEXES|KEYS} {FROM|IN} tbl [{FROM|IN} db]`. Returns
/// `None` for any other statement, including the `... WHERE`/`LIKE` filter form
/// (not handled), so those fall through to the parser.
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
    // A trailing `WHERE`/`LIKE` is not handled here.
    if k != toks.len() {
        return None;
    }
    Some(ShowIndex { table })
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
    Some(ShowColumns { full: false, table })
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

// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Minimal handling of the session and introspection queries that MySQL client
//! libraries issue during connection setup — `SELECT @@max_allowed_packet`,
//! `SET autocommit=1`, `SELECT VERSION()`, and friends.
//!
//! These are protocol glue, not user SQL: a client cannot finish connecting
//! until the server answers them. They are handled here, ahead of (and instead
//! of) the `CREATE TABLE`-only front-end parser, so that real MySQL clients can
//! connect and reach the statements the parser does support.

/// A canned response to a recognized session query.
pub enum SessionResponse {
    /// Reply with a bare OK packet (no result set).
    Ok,
    /// Reply with a single-row result set.
    Row {
        columns: Vec<String>,
        values: Vec<Option<String>>,
    },
}

/// Tries to handle `sql` as a session/introspection query. Returns `None` if it
/// is a real statement that should go to the parser.
pub fn try_handle(sql: &str) -> Option<SessionResponse> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let upper = trimmed.to_ascii_uppercase();

    // `SET ...` (autocommit, NAMES, sql_mode, ...) is acknowledged as a no-op.
    if upper == "SET" || upper.starts_with("SET ") {
        return Some(SessionResponse::Ok);
    }

    // `LOCK TABLE[S] ...` / `UNLOCK TABLE[S]` are no-ops: the engine is a single
    // writer, so the table locks MySQL uses to serialize access are unnecessary.
    // Acknowledge with OK. (MySQL's `LOCK TABLES` also commits an active
    // transaction and confines the session to the locked tables; neither of those
    // side effects is reproduced — a documented divergence.)
    let words: Vec<&str> = upper.split_whitespace().collect();
    if matches!(words.first(), Some(&"LOCK") | Some(&"UNLOCK"))
        && matches!(words.get(1), Some(&"TABLES") | Some(&"TABLE"))
    {
        return Some(SessionResponse::Ok);
    }

    // `SELECT @@var, ...` / `SELECT VERSION()` and similar introspection.
    if upper.starts_with("SELECT ") || upper.starts_with("SELECT\t") {
        return handle_select(&trimmed[6..]);
    }

    None
}

/// Handles a `SELECT` whose every item is a system variable or a known
/// session function. Returns `None` if any item is something else, so genuine
/// `SELECT` statements fall through to the parser.
fn handle_select(list: &str) -> Option<SessionResponse> {
    let list = strip_trailing_limit(list).trim();
    if list.is_empty() {
        return None;
    }
    let mut columns = Vec::new();
    let mut values = Vec::new();
    for item in split_top_level_commas(list) {
        let (expr, alias) = split_alias(item.trim());
        let value = eval(expr)?;
        columns.push(alias.unwrap_or_else(|| expr.to_string()));
        values.push(value);
    }
    Some(SessionResponse::Row { columns, values })
}

/// Evaluates a single session expression. The outer `Option` is `None` when the
/// expression is not one we handle; the inner `Option` distinguishes a value
/// from SQL `NULL`.
fn eval(expr: &str) -> Option<Option<String>> {
    let expr = expr.trim();
    if let Some(var) = expr.strip_prefix("@@") {
        let name = var
            .trim_start_matches("session.")
            .trim_start_matches("SESSION.")
            .trim_start_matches("global.")
            .trim_start_matches("GLOBAL.");
        return Some(Some(system_variable(name)));
    }
    match expr.to_ascii_uppercase().as_str() {
        "VERSION()" => Some(Some(SERVER_VERSION.to_string())),
        "DATABASE()" | "SCHEMA()" => Some(None),
        "CONNECTION_ID()" => Some(Some("1".to_string())),
        "USER()" | "CURRENT_USER()" | "SESSION_USER()" | "SYSTEM_USER()" => {
            Some(Some("root@localhost".to_string()))
        }
        _ => None,
    }
}

const SERVER_VERSION: &str = "8.0.0-turso";

/// The MySQL system variables the front-end reports plausible constant values
/// for, as `(name, value)` pairs. Values are rendered as text (the text protocol
/// sends everything as strings). This is the single source of truth for both
/// `SELECT @@var` (here) and `SHOW VARIABLES [LIKE ...]` (in `show.rs`).
pub const SYSTEM_VARIABLES: &[(&str, &str)] = &[
    ("autocommit", "1"),
    ("character_set_client", "utf8mb4"),
    ("character_set_connection", "utf8mb4"),
    ("character_set_database", "utf8mb4"),
    ("character_set_results", "utf8mb4"),
    ("character_set_server", "utf8mb4"),
    ("collation_connection", "utf8mb4_general_ci"),
    ("collation_database", "utf8mb4_general_ci"),
    ("collation_server", "utf8mb4_general_ci"),
    ("hostname", "turso"),
    ("init_connect", ""),
    ("interactive_timeout", "28800"),
    ("license", "MIT"),
    ("lower_case_table_names", "0"),
    ("max_allowed_packet", "67108864"),
    ("max_execution_time", "0"),
    ("net_read_timeout", "30"),
    ("net_write_timeout", "60"),
    ("performance_schema", "0"),
    ("protocol_version", "10"),
    ("sql_mode", ""),
    ("system_time_zone", "UTC"),
    ("time_zone", "SYSTEM"),
    ("transaction_isolation", "REPEATABLE-READ"),
    ("transaction_read_only", "0"),
    ("tx_isolation", "REPEATABLE-READ"),
    ("tx_read_only", "0"),
    ("version", SERVER_VERSION),
    ("version_comment", "Turso MySQL front-end"),
    ("version_compile_os", "Linux"),
    ("wait_timeout", "28800"),
];

/// Returns a plausible value for a MySQL system variable. Values are rendered as
/// text (the text protocol sends everything as strings); unknown variables yield
/// an empty string.
fn system_variable(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    SYSTEM_VARIABLES
        .iter()
        .find(|(n, _)| *n == lower)
        .map(|(_, v)| (*v).to_string())
        .unwrap_or_default()
}

/// Drops a trailing `LIMIT ...` clause (clients append `LIMIT 1` to some probes).
fn strip_trailing_limit(s: &str) -> &str {
    match s.to_ascii_uppercase().rfind(" LIMIT ") {
        Some(pos) => &s[..pos],
        None => s,
    }
}

/// Splits on top-level commas, respecting parentheses (for function arguments).
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                items.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&s[start..]);
    items
}

/// Splits an `expr AS alias` item into its expression and optional alias.
fn split_alias(item: &str) -> (&str, Option<String>) {
    match item.to_ascii_uppercase().rfind(" AS ") {
        Some(pos) => {
            let expr = item[..pos].trim();
            let alias = item[pos + 4..].trim().trim_matches('`').to_string();
            (expr, Some(alias))
        }
        None => (item.trim(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_is_ok() {
        assert!(matches!(
            try_handle("SET autocommit=1"),
            Some(SessionResponse::Ok)
        ));
        assert!(matches!(
            try_handle("SET NAMES utf8mb4"),
            Some(SessionResponse::Ok)
        ));
    }

    #[test]
    fn system_variable_select() {
        let Some(SessionResponse::Row { columns, values }) =
            try_handle("SELECT @@max_allowed_packet")
        else {
            panic!("expected a row");
        };
        assert_eq!(columns, vec!["@@max_allowed_packet"]);
        assert_eq!(values, vec![Some("67108864".to_string())]);
    }

    #[test]
    fn multi_variable_select_with_aliases() {
        let Some(SessionResponse::Row { columns, values }) =
            try_handle("SELECT @@max_allowed_packet AS p, @@wait_timeout AS w")
        else {
            panic!("expected a row");
        };
        assert_eq!(columns, vec!["p", "w"]);
        assert_eq!(
            values,
            vec![Some("67108864".to_string()), Some("28800".to_string())]
        );
    }

    #[test]
    fn real_select_falls_through() {
        assert!(try_handle("SELECT id FROM users").is_none());
        assert!(try_handle("SELECT 1").is_none());
    }

    #[test]
    fn lock_tables_is_ok() {
        for sql in [
            "LOCK TABLES wp_options WRITE",
            "LOCK TABLES a READ, b WRITE",
            "LOCK TABLE t WRITE",
            "UNLOCK TABLES",
            "UNLOCK TABLE",
        ] {
            assert!(
                matches!(try_handle(sql), Some(SessionResponse::Ok)),
                "expected `{sql}` to be acknowledged"
            );
        }
        // `LOCK INSTANCE` / `LOCK TABLESPACE` are not table locks and fall through.
        assert!(try_handle("LOCK INSTANCE FOR BACKUP").is_none());
    }
}

// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Minimal handling of the session and introspection queries that MySQL client
//! libraries issue during connection setup — `SELECT @@max_allowed_packet`,
//! `SET autocommit=1`, `SELECT VERSION()`, and friends.
//!
//! These are protocol glue, not user SQL: a client cannot finish connecting
//! until the server answers them. They are handled here, ahead of (and instead
//! of) the `CREATE TABLE`-only front-end parser, so that real MySQL clients can
//! connect and reach the statements the parser does support.

// The system-variable table lives in the parser crate (the single source of
// truth) so the parser can fold an `@@var` reference inside an expression to its
// value; re-export it here for the `SHOW VARIABLES` path in `show.rs`.
pub use turso_mysql_parser::{system_variable_value, SYSTEM_VARIABLES};
use turso_mysql_parser::SERVER_VERSION;

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

    // `FLUSH ...` (TABLES, PRIVILEGES, STATUS, LOGS, HOSTS, TABLES WITH READ LOCK,
    // ...) flushes server-side caches and state this single-node engine does not
    // keep, so it is a no-op that reports success — matching MySQL's empty OK.
    // It appears in mysqldump output and DB-admin / import flows. (Bare `FLUSH`
    // with no target is left to the parser, which rejects it as MySQL does.)
    if words.first() == Some(&"FLUSH") && words.len() >= 2 {
        return Some(SessionResponse::Ok);
    }

    // `DROP {PROCEDURE|FUNCTION} IF EXISTS name` is a no-op that reports success:
    // the engine has no stored routines, so there is never one to drop, and
    // `IF EXISTS` makes the drop succeed silently (MySQL only adds a note). This
    // is mysqldump's and a migration's standard "drop before create" preamble.
    // The bare form (no `IF EXISTS`) is left to the parser, which rejects it.
    if words.first() == Some(&"DROP")
        && matches!(words.get(1), Some(&"PROCEDURE") | Some(&"FUNCTION"))
        && words.get(2) == Some(&"IF")
        && words.get(3) == Some(&"EXISTS")
        && words.len() >= 5
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
        // Only a bare `@@var` reference is answered here; `@@var <op> ...` and
        // other expressions over it are not a plain variable name, so let them
        // fall through to the parser (which folds `@@var` to its value).
        if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Some(Some(system_variable(name)));
        }
        return None;
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

/// Returns a plausible value for a MySQL system variable. Values are rendered as
/// text (the text protocol sends everything as strings); unknown variables yield
/// an empty string.
fn system_variable(name: &str) -> String {
    system_variable_value(name).unwrap_or_default().to_string()
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
    fn flush_is_a_noop() {
        for sql in [
            "FLUSH TABLES",
            "FLUSH PRIVILEGES",
            "flush status;",
            "FLUSH TABLES WITH READ LOCK",
        ] {
            assert!(
                matches!(try_handle(sql), Some(SessionResponse::Ok)),
                "expected `{sql}` to be a no-op OK"
            );
        }
        // Bare `FLUSH` (no target) is left to the parser, which rejects it.
        assert!(try_handle("FLUSH").is_none());
    }

    #[test]
    fn drop_routine_if_exists_is_a_noop() {
        for sql in [
            "DROP PROCEDURE IF EXISTS foo",
            "DROP FUNCTION IF EXISTS foo",
            "drop procedure if exists `my proc`;",
            "DROP PROCEDURE IF EXISTS db.foo",
        ] {
            assert!(
                matches!(try_handle(sql), Some(SessionResponse::Ok)),
                "expected `{sql}` to be a no-op OK"
            );
        }
        // Without `IF EXISTS` (or a name), or for another object, it falls
        // through to the parser, which rejects it (matching MySQL, which errors
        // because the routine does not exist).
        for sql in [
            "DROP PROCEDURE foo",
            "DROP FUNCTION foo",
            "DROP PROCEDURE IF EXISTS",
            "DROP TABLE IF EXISTS foo",
        ] {
            assert!(try_handle(sql).is_none(), "expected `{sql}` to fall through");
        }
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
    fn data_loading_toggle_variables_default_on() {
        // mysqldump / import flows read these; both default to 1 in MySQL.
        for var in ["@@foreign_key_checks", "@@unique_checks"] {
            let Some(SessionResponse::Row { values, .. }) = try_handle(&format!("SELECT {var}"))
            else {
                panic!("expected a row for {var}");
            };
            assert_eq!(values, vec![Some("1".to_string())], "{var}");
        }
    }

    #[test]
    fn standard_default_system_variables() {
        // Clients / Site Health probe these; each reports MySQL's fixed 8.4
        // default. `SELECT @@var` renders booleans as 0/1, as MySQL does there.
        for (var, expected) in [
            ("@@default_storage_engine", "InnoDB"),
            ("@@sql_auto_is_null", "0"),
            ("@@sql_big_selects", "1"),
            ("@@group_concat_max_len", "1024"),
            ("@@net_buffer_length", "16384"),
            ("@@character_set_system", "utf8mb3"),
            ("@@sql_select_limit", "18446744073709551615"),
            ("@@have_query_cache", "NO"),
        ] {
            let Some(SessionResponse::Row { values, .. }) = try_handle(&format!("SELECT {var}"))
            else {
                panic!("expected a row for {var}");
            };
            assert_eq!(values, vec![Some(expected.to_string())], "{var}");
        }
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

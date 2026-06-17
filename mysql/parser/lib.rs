// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! A parser for the MySQL SQL dialect.
//!
//! It deliberately reuses the Turso engine's abstract syntax tree: parsing a
//! MySQL statement yields a [`turso_parser::ast::Stmt`]. That keeps a single AST
//! across the whole system — the MySQL front-end parses into it, and the engine
//! plans and executes from it (and can render it back to SQL). Unsupported
//! constructs are reported as [`error::ParseError::Unsupported`].
//!
//! # Example
//!
//! ```
//! use turso_mysql_parser::{parse, ast};
//!
//! let stmt = parse("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(80))").unwrap();
//! assert!(matches!(stmt, ast::Stmt::CreateTable { .. }));
//!
//! let stmt = parse("DROP TABLE IF EXISTS t").unwrap();
//! assert!(matches!(stmt, ast::Stmt::DropTable { if_exists: true, .. }));
//!
//! let stmt = parse("SELECT t.id FROM t1 AS t JOIN t2 ON t.id = t2.id").unwrap();
//! assert!(matches!(stmt, ast::Stmt::Select(_)));
//!
//! // Unsupported statements (and sub-clauses) are reported, not forwarded.
//! assert!(parse("SELECT a FROM t1 FULL JOIN t2 ON t1.id = t2.id").is_err());
//! ```

pub mod error;
pub mod lexer;
pub mod parser;
pub mod token;

/// Re-export of the shared AST this parser emits.
pub use turso_parser::ast;

pub use error::{ParseError, Result};
pub use parser::Parser;

/// The version string the front-end reports (for `VERSION()`, `@@version`, and
/// `SHOW VARIABLES`).
pub const SERVER_VERSION: &str = "8.0.0-turso";

/// The MySQL system variables the front-end reports plausible constant values
/// for, as `(name, value)` pairs. Values are rendered as text (the text protocol
/// sends everything as strings). This is the single source of truth shared by the
/// server (`SELECT @@var`, `SHOW VARIABLES [LIKE ...]`) and the parser, which
/// folds an `@@var` reference inside an expression to its value.
pub const SYSTEM_VARIABLES: &[(&str, &str)] = &[
    ("autocommit", "1"),
    ("big_tables", "0"),
    ("character_set_client", "utf8mb4"),
    ("character_set_connection", "utf8mb4"),
    ("character_set_database", "utf8mb4"),
    ("character_set_results", "utf8mb4"),
    ("character_set_server", "utf8mb4"),
    ("character_set_system", "utf8mb3"),
    ("collation_connection", "utf8mb4_general_ci"),
    ("collation_database", "utf8mb4_general_ci"),
    ("collation_server", "utf8mb4_general_ci"),
    ("default_storage_engine", "InnoDB"),
    ("default_tmp_storage_engine", "InnoDB"),
    ("foreign_key_checks", "1"),
    ("group_concat_max_len", "1024"),
    ("have_query_cache", "NO"),
    ("hostname", "turso"),
    ("init_connect", ""),
    ("innodb_strict_mode", "1"),
    ("interactive_timeout", "28800"),
    ("license", "MIT"),
    ("lower_case_table_names", "0"),
    ("max_allowed_packet", "67108864"),
    ("max_execution_time", "0"),
    ("net_buffer_length", "16384"),
    ("net_read_timeout", "30"),
    ("net_write_timeout", "60"),
    ("performance_schema", "0"),
    ("protocol_version", "10"),
    ("sql_auto_is_null", "0"),
    ("sql_big_selects", "1"),
    ("sql_mode", ""),
    ("sql_notes", "1"),
    ("sql_safe_updates", "0"),
    ("sql_select_limit", "18446744073709551615"),
    ("sql_warnings", "0"),
    ("system_time_zone", "UTC"),
    ("time_zone", "SYSTEM"),
    ("transaction_isolation", "REPEATABLE-READ"),
    ("transaction_read_only", "0"),
    ("tx_isolation", "REPEATABLE-READ"),
    ("tx_read_only", "0"),
    ("unique_checks", "1"),
    ("version", SERVER_VERSION),
    ("version_comment", "Turso MySQL front-end"),
    ("version_compile_os", "Linux"),
    ("wait_timeout", "28800"),
];

/// The constant value of a MySQL system variable (with any `session.` / `global.`
/// scope prefix already stripped, name compared case-insensitively), or `None`
/// if the front-end does not model it. Shared by the server and the parser's
/// `@@var` expression folding.
pub fn system_variable_value(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    SYSTEM_VARIABLES
        .iter()
        .find(|(n, _)| *n == lower)
        .map(|(_, v)| *v)
}

/// Parses a single MySQL statement into the shared [`ast::Stmt`].
pub fn parse(sql: &str) -> Result<ast::Stmt> {
    Parser::new(sql.as_bytes())?.parse_statement()
}

/// Parses MySQL into one or more statements. Identical to [`parse`] except that a
/// multi-table `DROP TABLE a, b, ...` — which has no single-statement engine form
/// — is expanded into one `DROP TABLE` per table for the caller to run in
/// sequence. Every other input yields a single-element vector.
pub fn parse_all(sql: &str) -> Result<Vec<ast::Stmt>> {
    Parser::new(sql.as_bytes())?.parse_statement_list()
}

/// Like [`parse_all`], but with the connection's current database name so
/// `DATABASE()`/`SCHEMA()` fold to it instead of `NULL` (`None` → `NULL`).
pub fn parse_all_in_db(sql: &str, current_db: Option<&str>) -> Result<Vec<ast::Stmt>> {
    Parser::new(sql.as_bytes())?
        .with_current_database(current_db.map(str::to_owned))
        .parse_statement_list()
}

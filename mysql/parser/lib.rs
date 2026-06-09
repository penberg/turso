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
//! let stmt = parse("DROP TABLE t").unwrap();
//! assert!(matches!(stmt, ast::Stmt::DropTable { .. }));
//!
//! let stmt = parse("SELECT id FROM users WHERE id = 1").unwrap();
//! assert!(matches!(stmt, ast::Stmt::Select(_)));
//!
//! // Unsupported statements (and sub-clauses) are reported, not forwarded.
//! assert!(parse("SELECT DISTINCT a FROM t").is_err());
//! assert!(parse("DROP TABLE IF EXISTS t").is_err());
//! ```

pub mod error;
pub mod lexer;
pub mod parser;
pub mod token;

/// Re-export of the shared AST this parser emits.
pub use turso_parser::ast;

pub use error::{ParseError, Result};
pub use parser::Parser;

/// Parses a single MySQL statement into the shared [`ast::Stmt`].
pub fn parse(sql: &str) -> Result<ast::Stmt> {
    Parser::new(sql.as_bytes())?.parse_statement()
}
